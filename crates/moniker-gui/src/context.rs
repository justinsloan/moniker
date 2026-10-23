//! State shared by every window and dialog ([`AppContext`]) and the way the app
//! tells the person something short ([`Notice`], shown in a window's
//! [`NotificationArea`] at the top center, or in the Preferences dialog while
//! that is open).
//!
//! The context also says when the profiles changed: whoever shows profiles
//! registers with [`AppContext::on_profiles_changed`], and
//! [`AppContext::profiles_changed`] (called after the app's own writes, and by
//! the file watch of [`AppContext::watch_profile_file`] after anyone's) reloads
//! the store once and tells them all. In the same way, whoever shows a sync's
//! progress registers with [`AppContext::on_sync_event`] and hears when any
//! sync starts and ends, whichever part of the app started it.
//!
//! Messages are plain text: they can contain a profile name or an error that
//! quotes one, and those come from sync data. The area shows them on a label, and
//! the Preferences dialog in a toast, both with Pango markup off.

use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};
use gtk4 as gtk;
use libadwaita as adw;
use moniker_core::secrets::{KeyringSecretStore, SecretStore};
use moniker_core::store::ProfileStore;
use moniker_core::transcript;

use crate::notify::{self, NotificationArea};
use crate::settings::{self, GuiSettings};
use crate::sync::SyncEvent;

/// How long the file watch waits after a change before reloading, so that the
/// burst of events one save makes (temp file, rename) reloads once.
const RELOAD_DELAY: Duration = Duration::from_millis(200);

/// Something to call when the profiles change (or a sync starts or ends),
/// alive while its owner is.
struct Listener<F: ?Sized> {
    owner: glib::WeakRef<glib::Object>,
    call: Rc<F>,
}

impl<F: ?Sized> Listener<F> {
    fn new(owner: &impl IsA<glib::Object>, call: Rc<F>) -> Self {
        Listener {
            owner: owner.upcast_ref::<glib::Object>().downgrade(),
            call,
        }
    }
}

/// What a sync listener is called with.
type SyncListener = dyn Fn(&SyncEvent);

/// The calls of the `listeners` whose owner still exists, copied out so a
/// call may register another listener (or cause another event) without a
/// borrow conflict. Forgets the rest.
fn live_calls<F: ?Sized>(listeners: &RefCell<Vec<Listener<F>>>) -> Vec<Rc<F>> {
    let mut listeners = listeners.borrow_mut();
    listeners.retain(|l| l.owner.upgrade().is_some());
    listeners.iter().map(|l| Rc::clone(&l.call)).collect()
}

/// Makes the worker function of a sync, in place of the real one. GUI tests
/// set it ([`AppContext::sync_runner`]) so that no test ever runs `ssh`.
#[cfg(test)]
pub(crate) type MakeRunner = Box<dyn Fn() -> crate::sync::Runner>;

/// State shared by every window and dialog of the application.
pub struct AppContext {
    pub store: RefCell<ProfileStore>,
    /// The keyring. The GUI calls it only on the keyring worker
    /// (`crate::keyring::run`), never on the main thread.
    pub secrets: Arc<dyn SecretStore>,
    /// The `moniker-askpass` helper that sits next to this executable.
    pub askpass_path: PathBuf,
    /// The per-user runtime directory (`XDG_RUNTIME_DIR`, absolute only), where
    /// the helper keeps each connection's one-time marker
    /// (`moniker_core::askpass`). A tab removes its marker from here when its
    /// session ends. `None`: the helper, which inherits this environment, cannot
    /// make a marker either, so it never sends a stored password and ssh asks in
    /// the terminal. Tests pass a temporary one.
    pub runtime_dir: Option<PathBuf>,
    /// Where short messages go. Each window attaches its notification area.
    pub notice: Notice,
    /// A sync is running. One runs at a time per app, whoever starts it (the
    /// sync page, Sync Now, the sync on launch): `sync::begin_app_sync`.
    pub sync_running: Cell<bool>,
    /// The GUI's settings file ([`settings`]): the sync partner and the
    /// automatic-sync switch. Beside the profile file.
    pub settings_path: PathBuf,
    /// Called by [`profiles_changed`](Self::profiles_changed).
    listeners: RefCell<Vec<Listener<dyn Fn()>>>,
    /// Called when a sync starts and ends ([`sync_event`](Self::sync_event)).
    sync_listeners: RefCell<Vec<Listener<SyncListener>>>,
    /// In tests, what every sync runs instead of the real `ssh` exchange
    /// (`sync::runner`). A test that starts a sync must set it.
    #[cfg(test)]
    pub(crate) sync_runner: RefCell<Option<MakeRunner>>,
    /// In tests, the secret store every sync's worker reads the sync key from
    /// instead of the system keyring (`sync::worker_secrets`). A test that
    /// starts a sync must set it.
    #[cfg(test)]
    pub(crate) sync_secrets: RefCell<Option<crate::sync::OpenSecrets>>,
    /// The watch on the profile file's directory, kept alive here.
    monitor: RefCell<Option<gio::FileMonitor>>,
    /// A reload is already scheduled by the file watch.
    reload_pending: Cell<bool>,
}

impl AppContext {
    /// Open the real profile store and the system keyring.
    ///
    /// Fails (with a message for the user) if the profile file cannot be
    /// located or read: a window running on a broken store would be worse than
    /// no window.
    pub fn new() -> Result<AppContext, String> {
        let path = ProfileStore::default_path();
        if !path.is_absolute() {
            return Err(format!(
                "cannot find a place for the profile file: {} is not an absolute path \
                 (HOME or XDG_CONFIG_HOME must be set to an absolute directory)",
                path.display()
            ));
        }
        let store = ProfileStore::open(&path)
            .map_err(|err| format!("cannot open the profile file {}: {err}", path.display()))?;
        let exe = std::env::current_exe()
            .map_err(|err| format!("cannot find the path of this program: {err}"))?;
        Ok(AppContext::with_paths(
            store,
            Arc::new(KeyringSecretStore::new()),
            exe.with_file_name("moniker-askpass"),
            settings::default_path(),
            runtime_dir_from(std::env::var_os("XDG_RUNTIME_DIR")),
        ))
    }

    /// A context over the given store, secret store and helper path, with no
    /// overlay attached and nobody listening yet. Tests use it with a
    /// temporary store and an in-memory secret store; the settings file is the
    /// one beside the store, as in the app. The secret store is watched: every
    /// call made on the thread that makes the context (the main thread, in the
    /// window tests) is counted (`keyring::MainThreadWatch`).
    #[cfg(test)]
    pub fn with_parts(
        store: ProfileStore,
        secrets: Arc<dyn SecretStore>,
        askpass_path: PathBuf,
    ) -> AppContext {
        let settings_path = store.path().with_file_name(settings::FILE_NAME);
        let secrets = Arc::new(crate::keyring::MainThreadWatch::new(secrets));
        AppContext::with_paths(store, secrets, askpass_path, settings_path, None)
    }

    /// This context with `dir` as its runtime directory (tests: a temporary
    /// one, never the real `XDG_RUNTIME_DIR`).
    #[cfg(test)]
    pub fn with_runtime_dir(self, dir: PathBuf) -> AppContext {
        AppContext {
            runtime_dir: Some(dir),
            ..self
        }
    }

    fn with_paths(
        store: ProfileStore,
        secrets: Arc<dyn SecretStore>,
        askpass_path: PathBuf,
        settings_path: PathBuf,
        runtime_dir: Option<PathBuf>,
    ) -> AppContext {
        AppContext {
            store: RefCell::new(store),
            secrets,
            askpass_path,
            runtime_dir,
            notice: Notice::default(),
            sync_running: Cell::new(false),
            settings_path,
            listeners: RefCell::new(Vec::new()),
            sync_listeners: RefCell::new(Vec::new()),
            #[cfg(test)]
            sync_runner: RefCell::new(None),
            #[cfg(test)]
            sync_secrets: RefCell::new(None),
            monitor: RefCell::new(None),
            reload_pending: Cell::new(false),
        }
    }

    /// The text size saved for the next launch ([`settings::GuiSettings::font_scale`]):
    /// where a tab starts in a window that has no tab to take its size from.
    /// Read from the file each time it is asked for (it only changes when the
    /// last window closes), so ask only when it is needed: a tab opened in a
    /// window with a selected tab takes that tab's size and does not ask
    /// (`terminal_tab::new_holder_page_at`).
    pub fn saved_font_scale(&self) -> f64 {
        settings::load(&self.settings_path).font_scale
    }

    /// The settings in the settings file, read now (a missing or damaged file is
    /// the defaults: [`settings::load`]). Nothing is cached, so a change made
    /// anywhere shows the next time this is asked.
    pub fn settings(&self) -> GuiSettings {
        settings::load(&self.settings_path)
    }

    /// Where transcripts are saved and listed: the folder chosen in the
    /// Preferences, else the default one under the data directory.
    pub fn transcripts_dir(&self) -> PathBuf {
        self.settings()
            .transcripts_dir
            .unwrap_or_else(transcript::default_dir)
    }

    /// Call `f` after every [`profiles_changed`](Self::profiles_changed), for as
    /// long as `owner` exists. `f` should hold its widgets weakly (or through
    /// `owner`'s own handlers): the context outlives every window.
    pub fn on_profiles_changed(&self, owner: &impl IsA<glib::Object>, f: impl Fn() + 'static) {
        self.listeners
            .borrow_mut()
            .push(Listener::new(owner, Rc::new(f)));
    }

    /// Call `f` when any sync starts and when it ends, for as long as `owner`
    /// exists. As with [`on_profiles_changed`](Self::on_profiles_changed), `f`
    /// should hold its widgets weakly.
    pub(crate) fn on_sync_event(
        &self,
        owner: &impl IsA<glib::Object>,
        f: impl Fn(&SyncEvent) + 'static,
    ) {
        self.sync_listeners
            .borrow_mut()
            .push(Listener::new(owner, Rc::new(f)));
    }

    /// Tell every sync listener whose owner still exists about `event`.
    pub(crate) fn sync_event(&self, event: &SyncEvent) {
        for call in live_calls(&self.sync_listeners) {
            call(event);
        }
    }

    /// The profile file may have changed: re-read it, then tell every listener
    /// whose owner still exists. If the file cannot be read the store keeps
    /// its last good snapshot, the person is told, and the listeners still run
    /// (they show that snapshot).
    pub fn profiles_changed(&self) {
        // Bind first so the store borrow ends before any listener runs.
        let reloaded = self.store.borrow_mut().reload();
        if let Err(err) = reloaded {
            self.notice
                .show(&format!("Could not re-read the profile file: {err}"));
        }
        for call in live_calls(&self.listeners) {
            call();
        }
    }

    /// Watch the profile file for changes made by anyone (a sync from this
    /// app, `moniker-server` in a terminal, another Moniker) and call
    /// [`profiles_changed`](Self::profiles_changed) after each, at most once per
    /// [`RELOAD_DELAY`].
    ///
    /// The watch is on the file's directory, not the file: saves replace the
    /// file by renaming a temporary one onto it, which a watch on the file
    /// itself would stop following. A watch that cannot be set up is reported
    /// on stderr; the app works without it, only without live updates.
    pub fn watch_profile_file(self: &Rc<Self>) {
        let path = self.store.borrow().path().to_path_buf();
        let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
            return;
        };
        let name = name.to_owned();
        let monitor = match gio::File::for_path(dir)
            .monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
        {
            Ok(monitor) => monitor,
            Err(err) => {
                eprintln!("moniker: cannot watch {} for changes: {err}", dir.display());
                return;
            }
        };
        // Weak: the context owns the monitor, which owns this closure.
        let ctx: Weak<AppContext> = Rc::downgrade(self);
        monitor.connect_changed(move |_, file, other, _event| {
            let is_profile_file =
                |f: Option<&gio::File>| f.and_then(|f| f.basename()).is_some_and(|b| *b == *name);
            if !(is_profile_file(Some(file)) || is_profile_file(other)) {
                return;
            }
            let Some(ctx) = ctx.upgrade() else {
                return;
            };
            if ctx.reload_pending.replace(true) {
                return;
            }
            let ctx = Rc::downgrade(&ctx);
            glib::timeout_add_local_once(RELOAD_DELAY, move || {
                if let Some(ctx) = ctx.upgrade() {
                    ctx.reload_pending.set(false);
                    ctx.profiles_changed();
                }
            });
        });
        self.monitor.replace(Some(monitor));
    }
}

/// The runtime directory named by `XDG_RUNTIME_DIR` (`value`), if it is an
/// absolute path; the helper ignores anything else too.
fn runtime_dir_from(value: Option<OsString>) -> Option<PathBuf> {
    value.map(PathBuf::from).filter(|dir| dir.is_absolute())
}

/// Short messages shown at the top center of a window (plain text, never
/// markup), or in the Preferences dialog while it is open
/// ([`show_over`](Notice::show_over)).
///
/// A cheap handle: clones share one list of notification areas, one per window
/// that wants messages ([`attach`](Notice::attach)). It holds them weakly, so
/// it keeps no window alive; a closed window's area is forgotten, and a
/// message with no live area is written to stderr instead (a transcripts move
/// can finish after the last window has closed, a transcript can fail to save
/// as the app quits).
#[derive(Clone, Default)]
pub struct Notice {
    inner: Rc<NoticeInner>,
}

#[derive(Default)]
struct NoticeInner {
    /// In the order they were attached.
    areas: RefCell<Vec<glib::WeakRef<NotificationArea>>>,
    /// What the last message was and where it went; tests read it instead of
    /// looking into the area or the dialog.
    #[cfg(test)]
    last: RefCell<Option<(Delivered, String)>>,
    /// How many messages reached an area or a dialog.
    #[cfg(test)]
    delivered: Cell<usize>,
    /// The messages that had no area to go to and were written to stderr.
    #[cfg(test)]
    printed: RefCell<Vec<String>>,
}

/// Where a message went.
#[cfg(test)]
enum Delivered {
    Area(glib::WeakRef<NotificationArea>),
    Dialog(glib::WeakRef<adw::PreferencesDialog>),
}

impl PartialEq for Notice {
    /// Two handles are equal if they are the same notice board.
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Notice {
    /// Let messages appear in `area`. Any number of windows may attach; the
    /// one attached last is the default place for [`show`](Notice::show).
    pub fn attach(&self, area: &NotificationArea) {
        let mut areas = self.inner.areas.borrow_mut();
        // Forget dead areas, and this one if it was attached before.
        areas.retain(|weak| weak.upgrade().is_some_and(|known| known != *area));
        areas.push(area.downgrade());
    }

    /// Show `text` in the active window if one of the attached areas is in it,
    /// else in the most recently attached area that is still alive, else (no
    /// window left to show it) on stderr.
    pub fn show(&self, text: &str) {
        let live = self.live();
        let in_active_window = live.iter().find(|area| {
            area.root()
                .and_downcast::<gtk::Window>()
                .is_some_and(|window| window.is_active())
        });
        match in_active_window.or(live.last()) {
            Some(area) => self.deliver(area, text),
            None => self.print(text),
        }
    }

    /// Show `text` in the area around `widget`, so it appears in the window the
    /// message is about. A widget in no area gets what [`show`](Notice::show)
    /// would do.
    pub fn show_near(&self, widget: &impl IsA<gtk::Widget>, text: &str) {
        let around = widget
            .ancestor(NotificationArea::static_type())
            .and_downcast::<NotificationArea>();
        match around {
            Some(area) => self.deliver(&area, text),
            None => self.show(text),
        }
    }

    /// Show `text` in the Preferences `dialog`, in its own message area, if it
    /// is open: the dialog covers the window's notification area, so its own
    /// messages (a setting that could not be saved, how a move went) must be
    /// seen there. Once it is closed or gone, this is what [`show`](Notice::show)
    /// does. The toast is plain text; it stays as long as an area would show
    /// the message.
    pub fn show_over(&self, dialog: &adw::PreferencesDialog, text: &str) {
        if !notify::is_shown(dialog.upcast_ref()) {
            self.show(text);
            return;
        }
        // Whole seconds, rounded up (`display_time` already is).
        let time = notify::display_time(text);
        let seconds = time.as_secs() + u64::from(time.subsec_nanos() > 0);
        dialog.add_toast(
            adw::Toast::builder()
                .title(text)
                .use_markup(false)
                .timeout(u32::try_from(seconds).unwrap_or(u32::MAX))
                .build(),
        );
        #[cfg(test)]
        self.delivered_to(Delivered::Dialog(dialog.downgrade()), text);
    }

    /// The attached areas that still exist, oldest first. Forgets the rest.
    fn live(&self) -> Vec<NotificationArea> {
        let mut live = Vec::new();
        self.inner
            .areas
            .borrow_mut()
            .retain(|weak| match weak.upgrade() {
                Some(area) => {
                    live.push(area);
                    true
                }
                None => false,
            });
        live
    }

    fn deliver(&self, area: &NotificationArea, text: &str) {
        area.show(text);
        #[cfg(test)]
        self.delivered_to(Delivered::Area(area.downgrade()), text);
    }

    /// A message with no window to show it in goes to stderr rather than
    /// nowhere: how a transcripts move went, or that a transcript could not be
    /// saved, still matters after the last window has closed. Messages are
    /// error and count lines, never a password, a startup command or a
    /// transcript's contents (they appear on screen otherwise), so stderr may
    /// have them as well.
    fn print(&self, text: &str) {
        eprintln!("moniker: {text}");
        #[cfg(test)]
        self.inner.printed.borrow_mut().push(text.to_owned());
    }

    /// Note, for tests, that `text` reached `to`.
    #[cfg(test)]
    fn delivered_to(&self, to: Delivered, text: &str) {
        self.inner.last.replace(Some((to, text.to_owned())));
        self.inner.delivered.set(self.inner.delivered.get() + 1);
    }

    /// How many messages have reached an area or the Preferences dialog so far.
    #[cfg(test)]
    pub(crate) fn delivered(&self) -> usize {
        self.inner.delivered.get()
    }

    /// The messages written to stderr, as there was no area to show them in.
    #[cfg(test)]
    pub(crate) fn printed(&self) -> Vec<String> {
        self.inner.printed.borrow().clone()
    }

    /// The text of the last message that reached an area or the dialog.
    #[cfg(test)]
    pub(crate) fn last_text(&self) -> Option<String> {
        self.inner
            .last
            .borrow()
            .as_ref()
            .map(|(_, text)| text.clone())
    }

    /// The area the last message went to, if it went to an area and that still
    /// exists.
    #[cfg(test)]
    pub(crate) fn last_area(&self) -> Option<NotificationArea> {
        match self.inner.last.borrow().as_ref() {
            Some((Delivered::Area(area), _)) => area.upgrade(),
            _ => None,
        }
    }

    /// The Preferences dialog the last message went to, if it went to the
    /// dialog and that still exists.
    #[cfg(test)]
    pub(crate) fn last_dialog(&self) -> Option<adw::PreferencesDialog> {
        match self.inner.last.borrow().as_ref() {
            Some((Delivered::Dialog(dialog), _)) => dialog.upgrade(),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moniker_core::secrets::MemorySecretStore;

    #[test]
    fn a_message_with_no_window_to_show_it_goes_to_stderr() {
        // A transcripts move that ends after the last window has closed: no
        // area is attached (or every one is gone), and the message is written
        // to stderr instead of vanishing.
        let notice = Notice::default();
        notice.show("Moved 2 transcripts.");
        assert_eq!(notice.printed(), ["Moved 2 transcripts."]);
        assert_eq!(notice.delivered(), 0, "it reached no area");
        assert_eq!(notice.last_text(), None);
    }

    #[test]
    fn only_an_absolute_runtime_dir_is_used() {
        assert_eq!(
            runtime_dir_from(Some("/run/user/1000".into())),
            Some(PathBuf::from("/run/user/1000"))
        );
        assert_eq!(runtime_dir_from(None), None);
        assert_eq!(runtime_dir_from(Some("".into())), None);
        assert_eq!(runtime_dir_from(Some("run/user/1000".into())), None);
    }

    /// A context over a store in a temporary directory, with its settings file
    /// (not yet written) beside it. No display is needed to build one.
    fn context_in(dir: &std::path::Path) -> AppContext {
        let store = ProfileStore::open(&dir.join("profiles.json")).unwrap();
        AppContext::with_parts(
            store,
            Arc::new(MemorySecretStore::default()),
            dir.join("askpass"),
        )
    }

    #[test]
    fn settings_are_read_from_the_file_each_time() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context_in(dir.path());
        assert_eq!(ctx.settings(), GuiSettings::default(), "no file yet");

        settings::update(&ctx.settings_path, |s| s.scrollback_lines = 2_000).unwrap();
        assert_eq!(ctx.settings().scrollback_lines, 2_000);
        settings::update(&ctx.settings_path, |s| s.save_all_transcripts = true).unwrap();
        let now = ctx.settings();
        assert!(now.save_all_transcripts);
        assert_eq!(now.scrollback_lines, 2_000);
    }

    #[test]
    fn transcripts_go_to_the_chosen_folder_else_the_default_one() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context_in(dir.path());
        assert_eq!(ctx.transcripts_dir(), transcript::default_dir());

        let chosen = dir.path().join("my transcripts");
        settings::update(&ctx.settings_path, |s| {
            s.transcripts_dir = Some(chosen.clone());
        })
        .unwrap();
        assert_eq!(ctx.transcripts_dir(), chosen);

        // A folder that is not an absolute path is no folder.
        std::fs::write(&ctx.settings_path, r#"{"transcripts_dir": "relative/dir"}"#).unwrap();
        assert_eq!(ctx.transcripts_dir(), transcript::default_dir());
    }
}
