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
//!   is not pinged (one reached through another host, one whose `ssh`
//!   configuration could not be read, or a system that does not allow pings).
//!   Different in shape as well as color, about the size of a lowercase letter
//!   (sized in `em`, so it follows the text size), no click of its own, no
//!   animation; its tooltip and accessible name say what it means
//!   ([`Reach::text`]), and the row's accessible description says it too.
//!   Drawn with CSS only.
//! * **A check** ([`real_check`]): nothing at all where pings are not allowed;
//!   otherwise where the connection really goes, from `ssh -G`
//!   (`moniker_core::ssh_config`, asked again at most every [`TARGET_FRESH`]
//!   per host, so `Match exec` commands in `~/.ssh/config` do not run every
//!   minute); a host reached through `ProxyJump` or `ProxyCommand`, or one
//!   `ssh -G` cannot tell about, is not pinged; then up to three pings
//!   (`moniker_core::ping`). On a worker thread, at most [`AT_ONCE`] at a
//!   time. A host keeps what its mark shows until a check decides, so nothing
//!   flickers.
//! * **When.** A mark that comes into view asks for a check of its host unless
//!   one decided less than [`FRESH`] ago; while any mark is in view, its hosts
//!   are checked again every [`INTERVAL`]. Nothing runs per frame.
//! * **Off** (the Preferences switch, `settings::GuiSettings::ping_hosts`): no
//!   mark is shown and no check starts; a check under way is called off before
//!   its next ping, and what it found is ignored; what was known is forgotten,
//!   so turning it on again starts from "not checked yet".
//!
//! The pings are the one network traffic Moniker makes besides the `ssh`
//! connections themselves, and they carry nothing from Moniker.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::{gdk, glib};
use gtk4 as gtk;
use libadwaita as adw;
use moniker_core::ping::{self, Ping};
use moniker_core::profile::Profile;
use moniker_core::ssh_config::{self, PingTarget};

/// A host checked less than this long ago is not checked again when a mark of
/// it comes into view.
pub(crate) const FRESH: Duration = Duration::from_secs(30);

/// How often the hosts of the marks in view are checked again.
pub(crate) const INTERVAL: Duration = Duration::from_secs(60);

/// How long what `ssh -G` said about a host is used before it is asked again.
pub(crate) const TARGET_FRESH: Duration = Duration::from_secs(5 * 60);

/// The most checks that run at once.
const AT_ONCE: usize = 4;

/// The CSS class every mark has.
pub(crate) const MARK_CLASS: &str = "host-mark";

/// How the marks are drawn: a dot, a ring and a dash of the same width, in the
/// color of the libadwaita style class each also has (`success`, `error`,
/// `dim-label`), so they follow the light, dark and high-contrast styles.
const CSS: &str = "
.host-mark { min-width: 0.55em; min-height: 0.55em; border-radius: 9999px; }
.host-mark.answers { background-color: currentColor; }
.host-mark.no-reply { min-width: 0.35em; min-height: 0.35em; border: 0.1em solid currentColor; }
.host-mark.unknown { min-height: 0.14em; border-radius: 0.07em; background-color: currentColor; }
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
    /// `ssh -G` could not say where the connection goes: not pinged.
    Unclear,
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
            Reach::Unclear => "Not checked: could not read the ssh configuration",
            Reach::NotAllowed => "Not checked: this system does not allow pings",
        }
    }

    /// The mark's shape class.
    pub(crate) fn shape(self) -> &'static str {
        match self {
            Reach::Answers => "answers",
            Reach::NoReply => "no-reply",
            Reach::Unknown | Reach::Proxied | Reach::Unclear | Reach::NotAllowed => "unknown",
        }
    }

    /// The libadwaita style class that gives the mark its color.
    fn color(self) -> &'static str {
        match self {
            Reach::Answers => "success",
            Reach::NoReply => "error",
            Reach::Unknown | Reach::Proxied | Reach::Unclear | Reach::NotAllowed => "dim-label",
        }
    }
}

/// What a check finds for a profile's host. It runs on a worker thread, and
/// asks the function it is given before each step that sends something
/// whether to go on (the checks may have been turned off meanwhile).
pub(crate) type Check = Arc<dyn Fn(&Profile, &dyn Fn() -> bool) -> Reach + Send + Sync>;

/// Where the connection to each host goes, as `ssh -G` said, and when.
type Targets = Mutex<HashMap<String, (Instant, Option<PingTarget>)>>;

/// The real check (module docs), with a memory of what `ssh -G` said. (Tests
/// never run it: `context::host_check`.)
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn real_check() -> Check {
    let targets: Arc<Targets> = Arc::default();
    Arc::new(move |profile, keep_going| {
        if !ping::allowed() {
            return Reach::NotAllowed;
        }
        let target = target_of(&targets, profile, Instant::now(), &|p| {
            ssh_config::effective_config(p)
        });
        match target {
            None => Reach::Unclear,
            Some(PingTarget::ThroughAnotherHost) => Reach::Proxied,
            Some(PingTarget::Host(host)) => match ping::ping(&host, keep_going) {
                Ping::Answered => Reach::Answers,
                Ping::NoReply => Reach::NoReply,
                Ping::NotAllowed => Reach::NotAllowed,
            },
        }
    })
}

/// Where the connection to `profile`'s host goes: what `resolve` (`ssh -G`)
/// said about it less than [`TARGET_FRESH`] before `now`, or what it says now.
/// A host `ssh -G` could not tell about is not asked again before then either.
fn target_of(
    targets: &Targets,
    profile: &Profile,
    now: Instant,
    resolve: &dyn Fn(&Profile) -> Option<String>,
) -> Option<PingTarget> {
    let key = host_key(&profile.host);
    let known = targets
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&key)
        .filter(|(at, _)| now.saturating_duration_since(*at) < TARGET_FRESH)
        .map(|(_, target)| target.clone());
    if let Some(target) = known {
        return target;
    }
    let target = resolve(profile).and_then(|dump| ssh_config::ping_target(&dump));
    targets
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(key, (now, target.clone()));
    target
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
    /// Every mark made, with its host. Dead ones are dropped as they are found.
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
    /// called off (it reads this on its thread) and ignored when it ends.
    generation: Arc<AtomicU64>,
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
            generation: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Turn the checks and the marks on or off (module docs).
    pub(crate) fn set_enabled(self: &Rc<Self>, on: bool) {
        if self.enabled.replace(on) == on {
            return;
        }
        if !on {
            self.generation.fetch_add(1, Ordering::SeqCst);
            self.queue.borrow_mut().clear();
            for host in self.hosts.borrow_mut().values_mut() {
                host.reach = Reach::Unknown;
                host.decided = None;
                host.checking = false;
            }
        }
        // Shown again, each mark in view asks for its check itself (`map`).
        for (key, mark) in self.live_marks() {
            self.style(&mark, &key);
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
        let mark: gtk::Widget = gtk::Box::builder()
            .accessible_role(gtk::AccessibleRole::Img)
            .valign(gtk::Align::Center)
            .halign(gtk::Align::Center)
            .css_classes([MARK_CLASS])
            .margin_end(2)
            .build()
            .upcast();
        self.style(&mark, &key);
        {
            let mut marks = self.marks.borrow_mut();
            marks.retain(|(_, mark)| mark.upgrade().is_some());
            marks.push((key.clone(), mark.downgrade()));
        }
        let weak = Rc::downgrade(self);
        mark.connect_map(move |mark| {
            if let Some(hosts) = weak.upgrade() {
                // In its row now: say it in the row's description too.
                hosts.style(mark, &key);
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

    /// Show on `mark` what is known about its host `key`, or nothing while off.
    fn style(&self, mark: &gtk::Widget, key: &str) {
        let reach = self.reach(key);
        let on = self.enabled.get();
        mark.set_visible(on);
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
        // A screen reader reads a row's title and description, not what is in
        // its prefix: the status goes after the subtitle there.
        if let Some(row) = mark
            .ancestor(adw::ActionRow::static_type())
            .and_downcast::<adw::ActionRow>()
        {
            let subtitle = row.subtitle().unwrap_or_default();
            let description = if on {
                format!("{subtitle}. {}", reach.text())
            } else {
                subtitle.to_string()
            };
            row.upcast_ref::<gtk::Widget>()
                .update_property(&[gtk::accessible::Property::Description(&description)]);
        }
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
            let generation = Arc::clone(&self.generation);
            let started_in = generation.load(Ordering::SeqCst);
            let (sender, received) = async_channel::bounded::<Reach>(1);
            let spawned = thread::Builder::new()
                .name("moniker-ping".to_owned())
                .spawn(move || {
                    let keep_going = || generation.load(Ordering::SeqCst) == started_in;
                    let _ = sender.send_blocking(check(&profile, &keep_going));
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
                    hosts.decided(&key, reach, started_in);
                }
            });
        }
    }

    /// A check of `key`, started in `generation`, found `reach`.
    fn decided(self: &Rc<Self>, key: &str, reach: Option<Reach>, generation: u64) {
        self.running.set(self.running.get().saturating_sub(1));
        if generation == self.generation.load(Ordering::SeqCst) {
            if let Some(host) = self.hosts.borrow_mut().get_mut(key) {
                host.checking = false;
                if let Some(reach) = reach {
                    host.reach = reach;
                    host.decided = Some(Instant::now());
                }
            }
            for (mark_key, mark) in self.live_marks() {
                if mark_key == key {
                    self.style(&mark, key);
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

    /// Tests: checks running now.
    #[cfg(test)]
    pub(crate) fn running(&self) -> usize {
        self.running.get()
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
        for reach in [
            Reach::Unknown,
            Reach::Proxied,
            Reach::Unclear,
            Reach::NotAllowed,
        ] {
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

    #[test]
    fn ssh_g_is_asked_again_only_after_a_while() {
        let targets = Targets::default();
        let asked = std::cell::Cell::new(0);
        let resolve = |_: &Profile| {
            asked.set(asked.get() + 1);
            Some("user alice\nhostname web1.example.com\n".to_owned())
        };
        let web = Profile::new("web", "Web", "alice");
        let start = Instant::now();
        let target = Some(PingTarget::Host("web1.example.com".to_owned()));
        assert_eq!(target_of(&targets, &web, start, &resolve), target);
        // The same host, spelled otherwise, a minute later: not asked again.
        let twin = Profile::new("twin", "WEB", "bob");
        let later = start + Duration::from_secs(60);
        assert_eq!(target_of(&targets, &twin, later, &resolve), target);
        assert_eq!(asked.get(), 1);
        // After TARGET_FRESH it is asked again.
        let much_later = start + TARGET_FRESH + Duration::from_secs(1);
        assert_eq!(target_of(&targets, &web, much_later, &resolve), target);
        assert_eq!(asked.get(), 2);
    }

    #[test]
    fn a_host_ssh_g_cannot_tell_about_is_not_asked_every_time() {
        let targets = Targets::default();
        let asked = std::cell::Cell::new(0);
        let fails = |_: &Profile| {
            asked.set(asked.get() + 1);
            None
        };
        let web = Profile::new("web", "web", "alice");
        let start = Instant::now();
        assert_eq!(target_of(&targets, &web, start, &fails), None);
        assert_eq!(
            target_of(&targets, &web, start + Duration::from_secs(60), &fails),
            None
        );
        assert_eq!(asked.get(), 1);
    }
}
