//! The host status of the profile lists (issue #5): whether each saved host
//! answers a ping from this machine right now, shown as a small mark before
//! the profile's name in the Connection Profiles sidebar and the new-tab list
//! (not on Local terminal or the Sync entry).
//!
//! * **One status per host for the whole app** ([`Hosts`], in the
//!   `AppContext`), shared by every list and window: a new tab does not ping
//!   every host again. Hosts are told apart by the profile's host, lowercased.
//! * **The mark** ([`Hosts::mark`]): a filled dot in the `success` color when
//!   the host answered, a ring in the `error` color when three pings got no
//!   reply, a short dimmed dash until a check has decided, and for a host that
//!   is not pinged (one reached through another host, or a system that does
//!   not allow pings). Different in shape as well as color, about the size of a
//!   lowercase letter, not clickable, no animation; its tooltip and accessible
//!   name say what it means ([`Reach::text`]). Drawn with CSS only.
//! * **A check** ([`check_host`]): `ssh -G` for where the connection really
//!   goes (`moniker_core::ssh_config`; a host behind `ProxyJump` or
//!   `ProxyCommand` is not pinged), then up to three pings
//!   (`moniker_core::ping`), on a worker thread, at most [`AT_ONCE`] at a time.
//!   A host keeps what its mark shows until a check decides, so nothing
//!   flickers.
//! * **When.** A mark that comes into view asks for a check of its host unless
//!   one decided less than [`FRESH`] ago; while any mark is in view, its hosts
//!   are checked again every [`INTERVAL`]. Nothing runs per frame.
//! * **Off** (the Preferences switch, `settings::GuiSettings::ping_hosts`): no
//!   mark is shown and no check starts; what was known is forgotten, so turning
//!   it on again starts from "not checked yet", and a check still running then
//!   is ignored when it ends.
//!
//! The pings are the one network traffic Moniker makes besides the `ssh`
//! connections themselves, and they carry nothing from Moniker.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::{gdk, glib};
use gtk4 as gtk;
use moniker_core::ping::{self, Ping};
use moniker_core::profile::Profile;
use moniker_core::ssh_config::{self, PingTarget};

/// A host checked less than this long ago is not checked again when a mark of
/// it comes into view.
pub(crate) const FRESH: Duration = Duration::from_secs(30);

/// How often the hosts of the marks in view are checked again.
pub(crate) const INTERVAL: Duration = Duration::from_secs(60);

/// The most checks that run at once.
const AT_ONCE: usize = 4;

/// The CSS class every mark has.
pub(crate) const MARK_CLASS: &str = "host-mark";

/// How the marks are drawn: a dot, a ring and a dash of the same width, in the
/// color of the libadwaita style class each also has (`success`, `error`,
/// `dim-label`), so they follow the light, dark and high-contrast styles.
const CSS: &str = "
.host-mark { min-width: 8px; min-height: 8px; border-radius: 9999px; }
.host-mark.answers { background-color: currentColor; }
.host-mark.no-reply { min-width: 5px; min-height: 5px; border: 1.5px solid currentColor; }
.host-mark.unknown { min-height: 2px; border-radius: 1px; background-color: currentColor; }
";

/// What is known about a host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reach {
    /// No check has decided yet.
    Unknown,
    /// It answered a ping.
    Answers,
    /// Three pings got no reply.
    NoReply,
    /// The connection goes through another host: not pinged.
    Proxied,
    /// This system does not allow pings: not pinged.
    NotAllowed,
}

impl Reach {
    /// The mark's tooltip and accessible name.
    pub(crate) fn text(self) -> &'static str {
        match self {
            Reach::Unknown => "Not checked yet",
            Reach::Answers => "Answers ping",
            Reach::NoReply => "No reply to ping",
            Reach::Proxied => "Not checked: reached through another host",
            Reach::NotAllowed => "Not checked: this system does not allow pings",
        }
    }

    /// The mark's shape class.
    pub(crate) fn shape(self) -> &'static str {
        match self {
            Reach::Answers => "answers",
            Reach::NoReply => "no-reply",
            Reach::Unknown | Reach::Proxied | Reach::NotAllowed => "unknown",
        }
    }

    /// The libadwaita style class that gives the mark its color.
    fn color(self) -> &'static str {
        match self {
            Reach::Answers => "success",
            Reach::NoReply => "error",
            Reach::Unknown | Reach::Proxied | Reach::NotAllowed => "dim-label",
        }
    }
}

/// What a check finds for a profile's host. It runs on a worker thread.
pub(crate) type Check = Arc<dyn Fn(&Profile) -> Reach + Send + Sync>;

/// The real check (module docs): where `ssh -G` says the connection goes, or
/// the profile's own host if it cannot tell, pinged from this machine. (Tests
/// never run it: `context::host_check`.)
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn check_host(profile: &Profile) -> Reach {
    let target =
        ssh_config::effective_config(profile).and_then(|dump| ssh_config::ping_target(&dump));
    let host = match target {
        Some(PingTarget::ThroughAnotherHost) => return Reach::Proxied,
        Some(PingTarget::Host(host)) => host,
        None => profile.host.clone(),
    };
    match ping::ping(&host) {
        Ping::Answered => Reach::Answers,
        Ping::NoReply => Reach::NoReply,
        Ping::NotAllowed => Reach::NotAllowed,
    }
}

/// How hosts are told apart: one status per host for the whole app.
pub(crate) fn host_key(host: &str) -> String {
    host.to_ascii_lowercase()
}

/// What is known about one host, and the profile a check of it uses (the one
/// whose mark was made last).
struct Host {
    reach: Reach,
    decided: Option<Instant>,
    checking: bool,
    profile: Profile,
}

/// The status of every host the lists show, for the whole app (module docs).
pub(crate) struct Hosts {
    enabled: Cell<bool>,
    check: RefCell<Check>,
    hosts: RefCell<HashMap<String, Host>>,
    /// Every mark made, by host. Dead ones are dropped as they are found.
    marks: RefCell<Vec<(String, glib::WeakRef<gtk::Widget>)>>,
    /// Hosts waiting for a check, in order.
    queue: RefCell<VecDeque<String>>,
    /// Checks running now.
    running: Cell<usize>,
    /// Marks in view (mapped) now.
    shown: Cell<usize>,
    /// The re-check every [`INTERVAL`], while marks are in view.
    timer: RefCell<Option<glib::SourceId>>,
    /// Moved on when the checks are turned off: a check started before is
    /// ignored when it ends.
    generation: Cell<u64>,
}

impl Hosts {
    /// No host known yet; checks with `check` once a mark comes into view, if
    /// `enabled`.
    pub(crate) fn new(enabled: bool, check: Check) -> Rc<Hosts> {
        Rc::new(Hosts {
            enabled: Cell::new(enabled),
            check: RefCell::new(check),
            hosts: RefCell::new(HashMap::new()),
            marks: RefCell::new(Vec::new()),
            queue: RefCell::new(VecDeque::new()),
            running: Cell::new(0),
            shown: Cell::new(0),
            timer: RefCell::new(None),
            generation: Cell::new(0),
        })
    }

    /// Turn the checks and the marks on or off (module docs).
    pub(crate) fn set_enabled(self: &Rc<Self>, on: bool) {
        if self.enabled.replace(on) == on {
            return;
        }
        if !on {
            self.generation.set(self.generation.get() + 1);
            self.queue.borrow_mut().clear();
            for host in self.hosts.borrow_mut().values_mut() {
                host.reach = Reach::Unknown;
                host.decided = None;
                host.checking = false;
            }
        }
        // Shown again, each mark in view asks for its check itself (`map`).
        for (_, mark) in self.live_marks() {
            self.style(&mark);
        }
    }

    /// What is known about `host` now.
    pub(crate) fn reach(&self, host: &str) -> Reach {
        self.hosts
            .borrow()
            .get(&host_key(host))
            .map_or(Reach::Unknown, |host| host.reach)
    }

    /// A new mark for `profile`'s host, to go before the profile's name in a
    /// list (module docs).
    pub(crate) fn mark(self: &Rc<Self>, profile: &Profile) -> gtk::Widget {
        add_css();
        let key = host_key(&profile.host);
        self.hosts
            .borrow_mut()
            .entry(key.clone())
            .and_modify(|host| host.profile = profile.clone())
            .or_insert_with(|| Host {
                reach: Reach::Unknown,
                decided: None,
                checking: false,
                profile: profile.clone(),
            });
        let mark = gtk::Box::builder()
            .accessible_role(gtk::AccessibleRole::Img)
            .valign(gtk::Align::Center)
            .halign(gtk::Align::Center)
            .css_classes([MARK_CLASS])
            .margin_end(2)
            .can_target(false)
            .build();
        mark.set_widget_name(&key);
        let mark: gtk::Widget = mark.upcast();
        self.style(&mark);
        self.marks
            .borrow_mut()
            .push((key.clone(), mark.downgrade()));
        let weak = Rc::downgrade(self);
        mark.connect_map(move |_| {
            if let Some(hosts) = weak.upgrade() {
                hosts.came_into_view(&key);
            }
        });
        let weak = Rc::downgrade(self);
        mark.connect_unmap(move |_| {
            if let Some(hosts) = weak.upgrade() {
                hosts.left_view();
            }
        });
        mark
    }

    /// Show on `mark` what is known about its host, or nothing while off.
    fn style(&self, mark: &gtk::Widget) {
        let reach = self.reach(&mark.widget_name());
        mark.set_visible(self.enabled.get());
        for class in [
            "answers",
            "no-reply",
            "unknown",
            "success",
            "error",
            "dim-label",
        ] {
            mark.remove_css_class(class);
        }
        mark.add_css_class(reach.shape());
        mark.add_css_class(reach.color());
        mark.set_tooltip_text(Some(reach.text()));
        mark.update_property(&[gtk::accessible::Property::Label(reach.text())]);
    }

    /// The marks still alive, with their hosts; the dead ones are forgotten.
    fn live_marks(&self) -> Vec<(String, gtk::Widget)> {
        let mut marks = self.marks.borrow_mut();
        marks.retain(|(_, mark)| mark.upgrade().is_some());
        marks
            .iter()
            .filter_map(|(key, mark)| mark.upgrade().map(|mark| (key.clone(), mark)))
            .collect()
    }

    fn came_into_view(self: &Rc<Self>, key: &str) {
        self.shown.set(self.shown.get() + 1);
        if self.timer.borrow().is_none() {
            let weak = Rc::downgrade(self);
            let timer = glib::timeout_add_local(INTERVAL, move || match weak.upgrade() {
                Some(hosts) => {
                    hosts.recheck_in_view();
                    glib::ControlFlow::Continue
                }
                None => glib::ControlFlow::Break,
            });
            self.timer.replace(Some(timer));
        }
        self.want(key);
    }

    fn left_view(&self) {
        self.shown.set(self.shown.get().saturating_sub(1));
        if self.shown.get() == 0
            && let Some(timer) = self.timer.take()
        {
            timer.remove();
        }
    }

    /// Ask again about the hosts of the marks in view (the [`INTERVAL`]).
    fn recheck_in_view(self: &Rc<Self>) {
        for (key, mark) in self.live_marks() {
            if mark.is_mapped() {
                self.want(&key);
            }
        }
    }

    /// Check `key` unless the checks are off, one is under way, or one decided
    /// less than [`FRESH`] ago.
    fn want(self: &Rc<Self>, key: &str) {
        if !self.enabled.get() {
            return;
        }
        {
            let mut hosts = self.hosts.borrow_mut();
            let Some(host) = hosts.get_mut(key) else {
                return;
            };
            if host.checking || host.decided.is_some_and(|at| at.elapsed() < FRESH) {
                return;
            }
            host.checking = true;
        }
        self.queue.borrow_mut().push_back(key.to_owned());
        self.start_checks();
    }

    /// Start waiting checks while fewer than [`AT_ONCE`] run.
    fn start_checks(self: &Rc<Self>) {
        while self.running.get() < AT_ONCE {
            let Some(key) = self.queue.borrow_mut().pop_front() else {
                return;
            };
            let Some(profile) = self.hosts.borrow().get(&key).map(|h| h.profile.clone()) else {
                continue;
            };
            let check = Arc::clone(&self.check.borrow());
            let generation = self.generation.get();
            let (sender, received) = async_channel::bounded::<Reach>(1);
            let spawned = thread::Builder::new()
                .name("moniker-ping".to_owned())
                .spawn(move || {
                    let _ = sender.send_blocking(check(&profile));
                });
            if spawned.is_err() {
                // Asked again the next time it is wanted.
                if let Some(host) = self.hosts.borrow_mut().get_mut(&key) {
                    host.checking = false;
                }
                return;
            }
            self.running.set(self.running.get() + 1);
            let weak = Rc::downgrade(self);
            glib::MainContext::default().spawn_local(async move {
                // `None`: the check panicked; the host stays as it was.
                let reach = received.recv().await.ok();
                if let Some(hosts) = weak.upgrade() {
                    hosts.decided(&key, reach, generation);
                }
            });
        }
    }

    /// A check of `key`, started in `generation`, found `reach`.
    fn decided(self: &Rc<Self>, key: &str, reach: Option<Reach>, generation: u64) {
        self.running.set(self.running.get().saturating_sub(1));
        if generation == self.generation.get() {
            if let Some(host) = self.hosts.borrow_mut().get_mut(key) {
                host.checking = false;
                if let Some(reach) = reach {
                    host.reach = reach;
                    host.decided = Some(Instant::now());
                }
            }
            for (mark_key, mark) in self.live_marks() {
                if mark_key == key {
                    self.style(&mark);
                }
            }
        }
        self.start_checks();
    }

    /// Tests: check with `check` from now on.
    #[cfg(test)]
    pub(crate) fn set_check(&self, check: Check) {
        self.check.replace(check);
    }

    /// Tests: forget when each host was last checked, so the next mark that
    /// comes into view, or the next [`INTERVAL`], checks it again.
    #[cfg(test)]
    pub(crate) fn make_stale(&self) {
        for host in self.hosts.borrow_mut().values_mut() {
            host.decided = None;
        }
    }

    /// Tests: what the [`INTERVAL`] does, now.
    #[cfg(test)]
    pub(crate) fn recheck_now(self: &Rc<Self>) {
        self.recheck_in_view();
    }

    /// Tests: whether the re-check timer runs.
    #[cfg(test)]
    pub(crate) fn timer_runs(&self) -> bool {
        self.timer.borrow().is_some()
    }
}

/// Load the marks' CSS on the default display, once.
fn add_css() {
    thread_local! {
        static ADDED: Cell<bool> = const { Cell::new(false) };
    }
    if ADDED.get() {
        return;
    }
    let Some(display) = gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    ADDED.set(true);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_state_reads_without_color() {
        // Shape as well as color: the three shapes, and a text for each.
        assert_eq!(Reach::Answers.shape(), "answers");
        assert_eq!(Reach::NoReply.shape(), "no-reply");
        for reach in [Reach::Unknown, Reach::Proxied, Reach::NotAllowed] {
            assert_eq!(reach.shape(), "unknown", "{reach:?}");
        }
        assert_eq!(Reach::Answers.text(), "Answers ping");
        assert_eq!(Reach::NoReply.text(), "No reply to ping");
        assert_eq!(Reach::Unknown.text(), "Not checked yet");
    }

    #[test]
    fn hosts_are_told_apart_by_their_lowercased_name() {
        assert_eq!(host_key("Web1.Example.COM"), "web1.example.com");
        assert_eq!(host_key("web1.example.com"), host_key("WEB1.example.com"));
    }
}
