//! Scenarios: the sync page of the Connection Profiles window, Sync Now and the automatic
//! sync on launch, driven with fake worker functions put in
//! `AppContext::sync_runner` (no ssh, no keyring).

use std::cell::{Cell, RefCell};
use std::fs;
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::{gio, glib};
use gtk4 as gtk;
use libadwaita as adw;
use moniker_core::profile::Profile;
use moniker_core::secrets::{
    MemorySecretStore, SYNC_PASSPHRASE_KEY, SecretError, SecretStore, password_key,
};
use moniker_core::store::ProfileStore;
use moniker_core::sync::{SyncError, SyncOutcome};
use zeroize::Zeroizing;

use super::main_window::test_app;
use super::{
    WAIT, all_widgets, button_labeled, find_widget, labels_in, make_helper, pump_for, pump_until,
    widgets_of_type,
};
use crate::context::AppContext;
use crate::new_tab;
use crate::profiles::{self, sync_page};
use crate::settings::{self, GuiSettings};
use crate::sync::{self, Runner, SyncInputs};
use crate::window::{self, MainWindow};

/// A sync key with spaces at both ends: it must be stored exactly as typed.
const SYNC_KEY: &str = "  correct horse battery  ";
const SYNCING: &str = "Syncing\u{2026} (can take up to two minutes)";
/// A profile name full of markup characters: shown as it is everywhere.
const NAME: &str = "<b>web</b> & _x_";

/// A handle on an in-memory keyring shared between the main thread (the
/// context's `secrets`) and the sync worker (`sync_secrets`), counting reads.
struct Shared {
    store: Arc<MemorySecretStore>,
    gets: Arc<AtomicUsize>,
}

impl SecretStore for Shared {
    fn get(&self, key: &str) -> Result<Option<Zeroizing<String>>, SecretError> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        self.store.get(key)
    }
    fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        self.store.set(key, value)
    }
    fn delete(&self, key: &str) -> Result<(), SecretError> {
        self.store.delete(key)
    }
    fn probe(&self) -> Result<(), SecretError> {
        self.store.probe()
    }
}

/// How often the main thread and the sync worker read the keyring.
#[derive(Default)]
struct Reads {
    main: Arc<AtomicUsize>,
    worker: Arc<AtomicUsize>,
}

/// A context over a temp store in `dir` (holding one profile with a markup
/// name if `with_profile`) and an in-memory keyring (holding `key` as the
/// sync key, if given) that the sync worker reads too.
fn sync_ctx(
    dir: &Path,
    with_profile: bool,
    key: Option<&str>,
) -> (Rc<AppContext>, Arc<MemorySecretStore>, Option<Profile>) {
    sync_ctx_counting(dir, with_profile, key, &Reads::default())
}

/// [`sync_ctx`], counting the keyring reads of each side in `reads`.
fn sync_ctx_counting(
    dir: &Path,
    with_profile: bool,
    key: Option<&str>,
    reads: &Reads,
) -> (Rc<AppContext>, Arc<MemorySecretStore>, Option<Profile>) {
    fs::create_dir_all(dir).expect("dir");
    let mut store = ProfileStore::open(&dir.join("profiles.json")).expect("open the store");
    let profile = with_profile.then(|| {
        let p = Profile::new(NAME, "app.example.test", "alice");
        store.upsert(p.clone()).expect("save the profile");
        store.get(&p.id).expect("saved").clone()
    });
    let secrets = Arc::new(MemorySecretStore::default());
    if let Some(key) = key {
        secrets.set(SYNC_PASSPHRASE_KEY, key).expect("set the key");
    }
    let ctx = Rc::new(AppContext::with_parts(
        store,
        Rc::new(Shared {
            store: Arc::clone(&secrets),
            gets: Arc::clone(&reads.main),
        }),
        make_helper(dir),
    ));
    let (store, gets) = (Arc::clone(&secrets), Arc::clone(&reads.worker));
    *ctx.sync_secrets.borrow_mut() = Some(Arc::new(move || {
        Box::new(Shared {
            store: Arc::clone(&store),
            gets: Arc::clone(&gets),
        })
    }));
    (ctx, secrets, profile)
}

/// Record `partner` as the sync partner, with the switch `auto_sync`.
fn set_partner(ctx: &AppContext, partner: &Profile, auto_sync: bool) {
    settings::save(
        &ctx.settings_path,
        &GuiSettings {
            sync_partner: Some(partner.id.clone()),
            auto_sync,
            ..GuiSettings::default()
        },
    )
    .expect("save the settings");
}

/// Make the next sync of `ctx` run `run`. A sync after that one, without a
/// new runner, panics on the worker (and so fails as "the sync stopped
/// unexpectedly").
fn set_runner(
    ctx: &AppContext,
    run: impl FnOnce(SyncInputs) -> Result<SyncOutcome, SyncError> + Send + 'static,
) {
    let slot: RefCell<Option<Runner>> = RefCell::new(Some(Box::new(run)));
    *ctx.sync_runner.borrow_mut() = Some(Box::new(move || {
        slot.borrow_mut().take().unwrap_or_else(|| {
            Box::new(|_: SyncInputs| -> Result<SyncOutcome, SyncError> {
                panic!("test: a sync ran without a runner of its own")
            })
        })
    }));
}

/// The sync page's widgets, found in the Connection Profiles window.
struct Page {
    widget: adw::PreferencesPage,
    key_entry: adw::PasswordEntryRow,
    key_status: gtk::Label,
    key_message: gtk::Label,
    machine: adw::ComboRow,
    auto_sync: adw::SwitchRow,
    hint: gtk::Label,
    sync_button: gtk::Button,
    result: gtk::Label,
}

/// The label below `root` whose widget name is `name`.
fn named_label(root: &impl IsA<gtk::Widget>, name: &str) -> gtk::Label {
    labels_in(root)
        .into_iter()
        .find(|l| l.widget_name() == name)
        .unwrap_or_else(|| panic!("no label {name}"))
}

fn page_in(window: &adw::Window) -> Page {
    let widget: adw::PreferencesPage = find_widget(window).expect("the sync page");
    Page {
        key_entry: find_widget(&widget).expect("the key field"),
        key_status: named_label(&widget, sync_page::KEY_STATUS_NAME),
        key_message: named_label(&widget, sync_page::KEY_MESSAGE_NAME),
        machine: find_widget(&widget).expect("the profile choice"),
        auto_sync: find_widget(&widget).expect("the switch"),
        hint: named_label(&widget, sync_page::HINT_NAME),
        sync_button: button_labeled(&widget, "Sync").expect("the Sync button"),
        result: named_label(&widget, sync_page::RESULT_NAME),
        widget,
    }
}

impl Page {
    /// The name the profile choice shows.
    fn chosen(&self) -> Option<String> {
        self.machine
            .selected_item()
            .and_downcast::<gtk::StringObject>()
            .map(|s| s.string().to_string())
    }

    fn choices(&self) -> u32 {
        self.machine.model().map_or(0, |m| m.n_items())
    }

    /// Press Sync with `run` as the worker function.
    fn sync_with(
        &self,
        ctx: &AppContext,
        run: impl FnOnce(SyncInputs) -> Result<SyncOutcome, SyncError> + Send + 'static,
    ) {
        set_runner(ctx, run);
        self.sync_button.emit_clicked();
    }
}

/// The sync page in a Connection Profiles window, with a counter of profile reloads.
struct SyncRig {
    app: adw::Application,
    ctx: Rc<AppContext>,
    secrets: Arc<MemorySecretStore>,
    synced: Rc<Cell<u32>>,
    window: adw::Window,
    page: Page,
    profile: Option<Profile>,
}

fn sync_rig(dir: &Path, with_profile: bool, key: Option<&str>) -> SyncRig {
    let (ctx, secrets, profile) = sync_ctx(dir, with_profile, key);
    let app = test_app();
    let synced = Rc::new(Cell::new(0));
    ctx.on_profiles_changed(&app, {
        let synced = Rc::clone(&synced);
        move || synced.set(synced.get() + 1)
    });
    let window = open_sync_page(&app, &ctx);
    let page = page_in(&window);
    SyncRig {
        app,
        ctx,
        secrets,
        synced,
        window,
        page,
        profile,
    }
}

/// The Connection Profiles window of `app`, on its sync page. Connecting from it does
/// nothing.
fn open_sync_page(app: &adw::Application, ctx: &Rc<AppContext>) -> adw::Window {
    let window = profiles::present_sync(app, ctx, Rc::new(|_: &Profile, _, _| {}));
    pump_for(Duration::from_millis(300));
    window
}

impl SyncRig {
    fn finish(self) {
        self.window.destroy();
        pump_for(Duration::from_millis(100));
    }
}

/// What a fake runner saw on the worker thread.
struct Seen {
    name: String,
    has_password: bool,
    passphrase: String,
    thread: ThreadId,
}

/// A fake runner: reports what it was given, waits, then returns `result`.
fn fake_runner(
    seen: mpsc::Sender<Seen>,
    runs: Arc<AtomicUsize>,
    delay: Duration,
    result: impl FnOnce() -> Result<SyncOutcome, SyncError> + Send + 'static,
) -> impl FnOnce(SyncInputs) -> Result<SyncOutcome, SyncError> + Send + 'static {
    move |inputs| {
        runs.fetch_add(1, Ordering::SeqCst);
        seen.send(Seen {
            name: inputs.profile.name.clone(),
            has_password: inputs.has_password,
            passphrase: inputs.passphrase.as_str().to_owned(),
            thread: thread::current().id(),
        })
        .expect("the test is listening");
        thread::sleep(delay);
        result()
    }
}

/// A runner that only counts and returns `result`.
fn counting(
    runs: &Arc<AtomicUsize>,
    result: impl FnOnce() -> Result<SyncOutcome, SyncError> + Send + 'static,
) -> impl FnOnce(SyncInputs) -> Result<SyncOutcome, SyncError> + Send + 'static {
    let runs = Arc::clone(runs);
    move |_| {
        runs.fetch_add(1, Ordering::SeqCst);
        result()
    }
}

fn added(n: usize) -> SyncOutcome {
    SyncOutcome {
        added: n,
        updated: 0,
        removed: 0,
    }
}

fn has_css(label: &gtk::Label, class: &str) -> bool {
    label.css_classes().iter().any(|c| c == class)
}

fn sync_now_enabled(app: &adw::Application) -> bool {
    app.lookup_action("sync-now")
        .expect("the sync-now action")
        .is_enabled()
}

// ----- the sync page (what the old sync dialog did) -----

pub(super) fn sync_page_key_and_availability(dir: &Path) {
    let rig = sync_rig(dir, true, None);
    let d = &rig.page;

    // No key yet: the status says so, Sync is off and says why.
    assert_eq!(d.key_status.label(), "Sync key: not set");
    assert!(!d.sync_button.is_sensitive());
    assert!(
        !d.machine.is_sensitive(),
        "the part waiting for the key is off"
    );
    assert!(d.hint.is_visible());
    assert_eq!(d.hint.label(), "Set a sync key first.");
    assert_eq!(
        d.machine.subtitle().as_deref(),
        Some("Connects to alice@app.example.test:22"),
        "the choice is spelled out"
    );
    // The choice shows the name as plain text, markup characters and all.
    let shown: Vec<(String, bool)> = labels_in(&d.machine)
        .iter()
        .map(|l| (l.label().to_string(), l.uses_markup()))
        .collect();
    assert!(shown.contains(&(NAME.to_owned(), false)), "{shown:?}");
    assert!(!d.machine.uses_markup());
    assert!(!d.auto_sync.uses_markup());

    // An empty key is refused inline and stores nothing.
    d.key_entry.emit_by_name::<()>("apply", &[]);
    assert_eq!(d.key_message.label(), "The sync key must not be empty.");
    assert!(d.key_message.is_visible());
    assert!(has_css(&d.key_message, "error"));
    assert!(rig.secrets.get(SYNC_PASSPHRASE_KEY).unwrap().is_none());

    // A key under the 12-character minimum is refused inline, stores nothing
    // and stays in the field (so it can be completed rather than retyped).
    assert!(
        d.key_entry.title().contains("12"),
        "the field names the minimum"
    );
    d.key_entry.set_text("only eleven");
    d.key_entry.emit_by_name::<()>("apply", &[]);
    assert_eq!(
        d.key_message.label(),
        "The sync key must be at least 12 characters long."
    );
    assert!(has_css(&d.key_message, "error"));
    assert_eq!(d.key_entry.text(), "only eleven");
    assert!(rig.secrets.get(SYNC_PASSPHRASE_KEY).unwrap().is_none());
    assert_eq!(d.key_status.label(), "Sync key: not set");
    assert!(!d.sync_button.is_sensitive());

    // Exactly 12 characters is accepted (and replaced below by SYNC_KEY).
    d.key_entry.set_text("twelve chars");
    d.key_entry.emit_by_name::<()>("apply", &[]);
    assert_eq!(
        rig.secrets
            .get(SYNC_PASSPHRASE_KEY)
            .unwrap()
            .expect("stored")
            .as_str(),
        "twelve chars"
    );
    assert!(!has_css(&d.key_message, "error"));
    rig.secrets.delete(SYNC_PASSPHRASE_KEY).unwrap();
    d.key_entry.emit_by_name::<()>("apply", &[]); // empty again: refused
    assert_eq!(d.key_message.label(), "The sync key must not be empty.");
    assert_eq!(d.key_status.label(), "Sync key: not set");

    // Saving stores it exactly as typed and clears the field.
    d.key_entry.set_text(SYNC_KEY);
    d.key_entry.emit_by_name::<()>("apply", &[]);
    let stored = rig
        .secrets
        .get(SYNC_PASSPHRASE_KEY)
        .unwrap()
        .expect("stored");
    assert_eq!(stored.as_str(), SYNC_KEY, "no trimming");
    assert_eq!(d.key_entry.text(), "", "the field is cleared after saving");
    assert_eq!(d.key_status.label(), "Sync key: set");
    assert_eq!(d.key_message.label(), "Sync key saved.");
    assert!(!has_css(&d.key_message, "error"));
    assert!(d.sync_button.is_sensitive(), "key and profile: Sync is on");
    assert!(d.machine.is_sensitive());
    assert!(!d.hint.is_visible());

    // Enter in the field saves too (and replaces the key). The row counts
    // only what is typed while it has focus as an edit to apply.
    d.key_entry.grab_focus();
    d.key_entry.set_text("second sync key");
    let text: gtk::Text = find_widget(&d.key_entry).expect("the field's text");
    text.emit_by_name::<()>("activate", &[]);
    let stored = rig
        .secrets
        .get(SYNC_PASSPHRASE_KEY)
        .unwrap()
        .expect("stored");
    assert_eq!(stored.as_str(), "second sync key");
    assert_eq!(d.key_entry.text(), "");
    pump_for(Duration::from_millis(300));
    assert!(
        !widgets_of_type(&d.key_entry, "GtkButton")
            .iter()
            .any(|b| b.has_css_class("apply-button") && b.is_child_visible()),
        "an emptied field has nothing to apply"
    );

    // Nothing in the window shows a key, and the lines that show changing
    // text do not interpret markup.
    for label in labels_in(&rig.window) {
        let text = label.label();
        assert!(
            !text.contains("correct horse") && !text.contains("second sync key"),
            "a label shows the key: {text:?}"
        );
    }
    for label in [&d.key_status, &d.key_message, &d.hint, &d.result] {
        assert!(!label.uses_markup(), "markup is on for {:?}", label.label());
    }

    // Choosing a profile instead drops the page, and with it a half-typed key.
    let profile = rig.profile.clone().expect("a profile");
    d.key_entry.set_text("half typed");
    let entry = d.key_entry.clone();
    let weak = d.widget.downgrade();
    let SyncRig {
        app,
        ctx,
        window,
        page,
        ..
    } = rig;
    drop(page);
    profiles::select(&window, &profile.id);
    pump_for(Duration::from_millis(100));
    assert_eq!(
        entry.text(),
        "",
        "the typed key is cleared when the page goes"
    );
    assert!(
        weak.upgrade().is_none(),
        "leaving the page must free it (no reference cycle)"
    );

    // Back on the page (the same window), a half-typed key is cleared when
    // the window closes, and the page is freed.
    let reopened = open_sync_page(&app, &ctx);
    assert_eq!(reopened, window, "one Connection Profiles window");
    let page = page_in(&window);
    page.key_entry.set_text("half typed");
    let entry = page.key_entry.clone();
    let weak = page.widget.downgrade();
    drop((page, reopened));
    window.close();
    assert_eq!(entry.text(), "", "the typed key is cleared on close");
    drop(window);
    pump_for(Duration::from_millis(100));
    assert!(
        weak.upgrade().is_none(),
        "closing the window frees the page"
    );
}

pub(super) fn sync_page_without_profiles(dir: &Path) {
    let rig = sync_rig(dir, false, Some(SYNC_KEY));
    let d = &rig.page;
    assert_eq!(d.key_status.label(), "Sync key: set");
    assert!(!d.machine.is_sensitive());
    assert!(!d.sync_button.is_sensitive());
    assert!(d.hint.is_visible());
    assert!(
        d.hint.label().contains("Add a profile"),
        "{}",
        d.hint.label()
    );
    assert_eq!(d.machine.subtitle().as_deref().unwrap_or(""), "");
    assert_eq!(d.chosen(), None);

    // A click that cannot happen from the UI is still harmless.
    let runs = Arc::new(AtomicUsize::new(0));
    d.sync_with(&rig.ctx, counting(&runs, || Ok(added(1))));
    pump_for(Duration::from_millis(100));
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert_eq!(d.result.label(), "Choose the other machine first.");
    rig.finish();
}

pub(super) fn sync_page_success(dir: &Path) {
    let rig = sync_rig(dir, true, Some(SYNC_KEY));
    let d = &rig.page;
    let profile = rig.profile.clone().expect("a profile");
    rig.secrets
        .set(&password_key(&profile.id), "pw-for-ssh")
        .unwrap();
    assert!(d.sync_button.is_sensitive());

    let runs = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = mpsc::channel();
    let started = Instant::now();
    d.sync_with(
        &rig.ctx,
        fake_runner(tx, Arc::clone(&runs), Duration::from_millis(400), || {
            Ok(added(1))
        }),
    );
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "starting a sync must not wait for it ({:?})",
        started.elapsed()
    );

    // While it runs: Sync and the choice are off, the label says so.
    assert!(rig.ctx.sync_running.get());
    assert!(!d.sync_button.is_sensitive());
    assert!(!d.machine.is_sensitive());
    assert_eq!(d.result.label(), SYNCING);
    assert!(!has_css(&d.result, "error"));
    // A second start while running does nothing.
    d.sync_with(
        &rig.ctx,
        fake_runner(mpsc::channel().0, Arc::clone(&runs), Duration::ZERO, || {
            Ok(added(9))
        }),
    );

    // The main loop keeps running while the worker sleeps.
    let ticked = Rc::new(Cell::new(false));
    glib::timeout_add_local_once(Duration::from_millis(50), {
        let ticked = Rc::clone(&ticked);
        move || ticked.set(true)
    });
    pump_until("the first sync to finish", || d.sync_button.is_sensitive());
    assert!(ticked.get(), "the UI was blocked while the worker ran");
    assert!(!rig.ctx.sync_running.get());

    assert_eq!(d.result.label(), "Synced: 1 added, 0 updated, 0 removed.");
    assert!(!has_css(&d.result, "error"));
    assert_eq!(rig.synced.get(), 1, "the profiles were reloaded once");
    assert_eq!(runs.load(Ordering::SeqCst), 1, "only one worker ran");
    assert!(d.machine.is_sensitive());

    // What the worker was handed: gathered on the main thread, key untouched.
    let seen = rx.try_recv().expect("the runner reported");
    assert_eq!(seen.name, NAME);
    assert!(seen.has_password);
    assert_eq!(seen.passphrase, SYNC_KEY);
    assert_ne!(seen.thread, thread::current().id(), "ran on a worker");

    // Nothing changed: said so.
    d.sync_with(&rig.ctx, counting(&runs, || Ok(SyncOutcome::default())));
    pump_until("the second sync", || d.sync_button.is_sensitive());
    assert_eq!(d.result.label(), "Already up to date.");
    assert_eq!(rig.synced.get(), 2);
    rig.finish();
}

pub(super) fn sync_page_errors(dir: &Path) {
    let rig = sync_rig(dir, true, Some(SYNC_KEY));
    let d = &rig.page;
    let runs = Arc::new(AtomicUsize::new(0));

    for (err, text) in [
        (
            SyncError::WrongSyncKey,
            "The other machine has a different sync key.",
        ),
        (
            SyncError::HostKeyUnknown,
            "Connect to this host once from a tab to trust its host key, then sync again.",
        ),
        (
            SyncError::ServerNotFound,
            "moniker-server isn't on the other machine's PATH.",
        ),
    ] {
        let (tx, rx) = mpsc::channel();
        d.sync_with(
            &rig.ctx,
            fake_runner(tx, Arc::clone(&runs), Duration::from_millis(100), || {
                Err(err)
            }),
        );
        assert!(!d.sync_button.is_sensitive());
        pump_until("a failing sync to finish", || d.sync_button.is_sensitive());
        assert_eq!(d.result.label(), text);
        assert!(has_css(&d.result, "error"), "errors are marked");
        assert!(rx.try_recv().is_ok());
    }
    assert_eq!(rig.synced.get(), 0, "a failed sync reloads nothing");
    assert!(
        !rig.ctx.settings_path.exists(),
        "a failed sync records no partner"
    );

    // A worker that panics is reported, and the page is usable again.
    d.sync_with(
        &rig.ctx,
        |_: SyncInputs| -> Result<SyncOutcome, SyncError> {
            panic!("test: the worker panics on purpose")
        },
    );
    pump_until("the panicking sync to finish", || {
        d.sync_button.is_sensitive()
    });
    assert_eq!(
        d.result.label(),
        "sync connection error: the sync stopped unexpectedly"
    );
    assert!(has_css(&d.result, "error"));
    assert!(!rig.ctx.sync_running.get(), "the app may sync again");

    // The next success clears the error marking.
    d.sync_with(&rig.ctx, counting(&runs, || Ok(added(2))));
    pump_until("a good sync", || d.sync_button.is_sensitive());
    assert_eq!(d.result.label(), "Synced: 2 added, 0 updated, 0 removed.");
    assert!(!has_css(&d.result, "error"));
    assert_eq!(rig.synced.get(), 1);

    // The key removed behind the page's back: the worker finds no key, the
    // exchange never runs, and the status line catches up.
    rig.secrets.delete(SYNC_PASSPHRASE_KEY).unwrap();
    let before = runs.load(Ordering::SeqCst);
    d.sync_with(&rig.ctx, counting(&runs, || Ok(added(1))));
    pump_until("the worker to find no key", || !rig.ctx.sync_running.get());
    assert_eq!(
        runs.load(Ordering::SeqCst),
        before,
        "the exchange never ran"
    );
    assert!(!rig.ctx.sync_running.get());
    assert_eq!(d.result.label(), "Set a sync key first.");
    assert_eq!(d.key_status.label(), "Sync key: not set");
    assert!(!d.sync_button.is_sensitive());

    // A stored password but no askpass helper: the sync does not start, the
    // page says why, and nothing was connected.
    let profile = rig.profile.clone().expect("a profile");
    rig.secrets.set(SYNC_PASSPHRASE_KEY, SYNC_KEY).unwrap();
    rig.secrets
        .set(&password_key(&profile.id), "pw-for-ssh")
        .unwrap();
    fs::remove_file(&rig.ctx.askpass_path).expect("remove the stand-in helper");
    let before = runs.load(Ordering::SeqCst);
    d.sync_with(&rig.ctx, counting(&runs, || Ok(added(1))));
    pump_until("the worker to find no helper", || {
        !rig.ctx.sync_running.get()
    });
    assert_eq!(
        runs.load(Ordering::SeqCst),
        before,
        "the exchange never ran"
    );
    assert_eq!(d.result.label(), sync::ASKPASS_MISSING_SYNC);
    assert!(has_css(&d.result, "error"));
    rig.finish();
}

pub(super) fn sync_page_profiles_change_during_a_sync(dir: &Path) {
    let rig = sync_rig(dir, true, Some(SYNC_KEY));
    let d = &rig.page;
    let profile = rig.profile.clone().expect("a profile");
    let path = rig.ctx.store.borrow().path().to_path_buf();
    assert_eq!(d.chosen().as_deref(), Some(NAME));

    // The worker writes a new profile into the file, as a real sync would.
    let runs = Arc::new(AtomicUsize::new(0));
    let runner = counting(&runs, || Ok(added(1)));
    d.sync_with(&rig.ctx, move |inputs: SyncInputs| {
        let mut store = ProfileStore::open(&inputs.store_path).expect("open");
        store
            .upsert(Profile::new("Alpha", "alpha.example.test", "bob"))
            .expect("upsert");
        thread::sleep(Duration::from_millis(50));
        runner(inputs)
    });
    pump_until("the sync", || d.sync_button.is_sensitive());
    assert_eq!(rig.synced.get(), 1);
    assert_eq!(
        rig.ctx.store.borrow().live().len(),
        2,
        "the store was reloaded"
    );
    assert_eq!(d.choices(), 2, "the choice lists the new profile");
    assert_eq!(
        d.chosen().as_deref(),
        Some(NAME),
        "the chosen profile is still chosen"
    );

    // A profile removed behind the page's back is refused, and the list
    // catches up.
    rig.ctx.store.borrow_mut().delete(&profile.id).unwrap();
    let before = runs.load(Ordering::SeqCst);
    d.sync_with(&rig.ctx, counting(&runs, || Ok(added(1))));
    pump_for(Duration::from_millis(100));
    assert_eq!(runs.load(Ordering::SeqCst), before, "no worker started");
    assert_eq!(d.result.label(), "That profile no longer exists.");
    assert_eq!(d.choices(), 1);
    assert_eq!(d.chosen().as_deref(), Some("Alpha"));
    assert!(path.exists());
    rig.finish();
}

pub(super) fn sync_page_closed_before_completion(dir: &Path) {
    // Success: the window is gone when the worker finishes. Nothing panics,
    // the profiles are still reloaded, and a notification says how it went.
    let rig = sync_rig(dir, true, Some(SYNC_KEY));
    let area = super::bare_area();
    rig.ctx.notice.attach(&area);
    let runs = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = mpsc::channel();
    rig.page.sync_with(
        &rig.ctx,
        fake_runner(tx, Arc::clone(&runs), Duration::from_millis(300), || {
            Ok(added(1))
        }),
    );
    rx.recv_timeout(WAIT).expect("the worker started");
    let weak = rig.page.widget.downgrade();
    let SyncRig {
        app,
        ctx,
        window,
        page,
        synced,
        ..
    } = rig;
    drop(page);
    window.close();
    drop(window);
    assert_eq!(synced.get(), 0, "still running");
    pump_until("the worker's result", || synced.get() == 1);
    pump_for(Duration::from_millis(100));
    assert!(weak.upgrade().is_none(), "the closed page was freed");
    assert_eq!(
        ctx.notice.last_text().as_deref(),
        Some("Synced: 1 added, 0 updated, 0 removed.")
    );
    assert_eq!(ctx.notice.last_area().as_ref(), Some(&area));

    // Failure: a notification says so; nothing is reloaded.
    let window = open_sync_page(&app, &ctx);
    let page = page_in(&window);
    let (tx, rx) = mpsc::channel();
    page.sync_with(
        &ctx,
        fake_runner(tx, Arc::clone(&runs), Duration::from_millis(200), || {
            Err(SyncError::WrongSyncKey)
        }),
    );
    rx.recv_timeout(WAIT).expect("the worker started");
    drop(page);
    window.close();
    drop(window);
    pump_until("the failure to be reported", || {
        ctx.notice
            .last_text()
            .is_some_and(|text| text.starts_with("Sync failed:"))
    });
    assert_eq!(
        ctx.notice.last_text().as_deref(),
        Some("Sync failed: The other machine has a different sync key.")
    );
    assert_eq!(synced.get(), 1, "a failed sync reloads nothing");
}

// ----- the partner, Sync Now and the sync on launch -----

/// (label, hidden-when) of the ☰ menu's item for `app.sync-now`.
fn sync_now_item(main: &MainWindow) -> Option<(String, String)> {
    fn find(menu: &gio::MenuModel) -> Option<(String, String)> {
        for i in 0..menu.n_items() {
            if let Some(section) = menu.item_link(i, gio::MENU_LINK_SECTION) {
                if let Some(found) = find(&section) {
                    return Some(found);
                }
                continue;
            }
            let attribute = |name: &str| {
                menu.item_attribute_value(i, name, None)
                    .and_then(|v| v.get::<String>())
                    .unwrap_or_default()
            };
            if attribute(gio::MENU_ATTRIBUTE_ACTION) == "app.sync-now" {
                return Some((
                    attribute(gio::MENU_ATTRIBUTE_LABEL),
                    attribute("hidden-when"),
                ));
            }
        }
        None
    }
    let model = widgets_of_type(&main.window, "GtkMenuButton")
        .into_iter()
        .filter_map(|w| w.downcast::<gtk::MenuButton>().ok())
        .find(|b| b.tooltip_text().as_deref() == Some("Main Menu"))
        .and_then(|b| b.menu_model())
        .expect("the main menu");
    find(&model)
}

pub(super) fn first_manual_success_enables_sync_now(dir: &Path) {
    let rig = sync_rig(dir, true, Some(SYNC_KEY));
    let profile = rig.profile.clone().expect("a profile");
    crate::add_actions(&rig.app, &rig.ctx);
    let main = window::build(&rig.app, &rig.ctx);
    main.window.present();
    pump_for(Duration::from_millis(200));
    assert_eq!(
        sync_now_item(&main),
        Some(("Sync Now".to_owned(), "action-disabled".to_owned())),
        "the menu item hides while its action is disabled"
    );
    assert!(!sync_now_enabled(&rig.app), "no partner: no Sync Now");
    assert_eq!(
        rig.page.auto_sync.subtitle().as_deref(),
        Some(sync_page::AUTO_SYNC_NO_PARTNER)
    );
    assert!(rig.page.auto_sync.is_active(), "on by default");

    // A failed sync records nothing.
    rig.page
        .sync_with(&rig.ctx, |_| Err(SyncError::WrongSyncKey));
    pump_until("the failed sync", || rig.page.sync_button.is_sensitive());
    assert!(!sync_now_enabled(&rig.app));
    assert!(!rig.ctx.settings_path.exists(), "nothing written");

    // The first success records the partner and offers Sync Now at once.
    rig.page.sync_with(&rig.ctx, |_| Ok(added(0)));
    pump_until("the sync", || rig.page.sync_button.is_sensitive());
    assert_eq!(
        settings::load(&rig.ctx.settings_path),
        GuiSettings {
            sync_partner: Some(profile.id.clone()),
            auto_sync: true,
            ..GuiSettings::default()
        }
    );
    assert!(sync_now_enabled(&rig.app), "Sync Now is offered");
    assert_eq!(
        rig.page.auto_sync.subtitle().as_deref(),
        Some(sync_page::auto_sync_with(NAME).as_str()),
        "the switch names the partner"
    );
    let text = fs::read_to_string(&rig.ctx.settings_path).expect("the settings file");
    assert!(
        !text.contains(SYNC_KEY.trim()),
        "the settings file holds no secret"
    );

    // The partner deleted (by a sync, say): Sync Now goes away again.
    rig.ctx.store.borrow_mut().delete(&profile.id).unwrap();
    rig.ctx.profiles_changed();
    assert!(!sync_now_enabled(&rig.app));
    main.window.destroy();
    rig.finish();
}

pub(super) fn sync_now_while_running_says_so(dir: &Path) {
    let (ctx, _secrets, profile) = sync_ctx(dir, true, Some(SYNC_KEY));
    let profile = profile.expect("a profile");
    set_partner(&ctx, &profile, true);
    let app = test_app();
    crate::add_actions(&app, &ctx);
    assert!(sync_now_enabled(&app));
    // The sync page is open: it follows a sync it did not start.
    let profiles_window = open_sync_page(&app, &ctx);
    let page = page_in(&profiles_window);
    let main = window::build(&app, &ctx);
    main.window.present();
    pump_for(Duration::from_millis(200));

    // The worker waits until the test lets it go.
    let (release, wait) = mpsc::channel::<()>();
    let wait = Mutex::new(wait);
    set_runner(&ctx, move |_| {
        wait.lock()
            .expect("the lock")
            .recv_timeout(WAIT)
            .expect("released");
        Ok(added(1))
    });
    app.activate_action("sync-now", None);
    assert!(ctx.sync_running.get());
    assert_eq!(page.result.label(), SYNCING, "the page shows it running");
    assert!(!page.sync_button.is_sensitive());

    // Asked again while it runs: it says so, in the window it was asked in.
    let delivered = ctx.notice.delivered();
    app.activate_action("sync-now", None);
    assert_eq!(
        ctx.notice.last_text().as_deref(),
        Some(sync::ALREADY_RUNNING)
    );
    assert_eq!(ctx.notice.last_area().as_ref(), Some(&main.notifications));
    assert_eq!(ctx.notice.delivered(), delivered + 1);
    // The page's own button (off anyway) starts nothing either.
    page.sync_button.emit_clicked();
    assert_eq!(page.result.label(), SYNCING);

    release.send(()).expect("the worker is waiting");
    pump_until("the sync to finish", || !ctx.sync_running.get());
    pump_for(Duration::from_millis(100));
    assert_eq!(
        ctx.notice.last_text().as_deref(),
        Some("Synced: 1 added, 0 updated, 0 removed."),
        "Sync Now says how it went"
    );
    assert_eq!(ctx.notice.last_area().as_ref(), Some(&main.notifications));
    assert_eq!(
        ctx.notice.delivered(),
        delivered + 2,
        "one notification each"
    );
    assert_eq!(
        page.result.label(),
        "Synced: 1 added, 0 updated, 0 removed."
    );
    assert!(page.sync_button.is_sensitive());

    // A failure says so too.
    set_runner(&ctx, |_| Err(SyncError::HostKeyUnknown));
    app.activate_action("sync-now", None);
    pump_until("the failed sync", || !ctx.sync_running.get());
    assert_eq!(
        ctx.notice.last_text().as_deref(),
        Some(
            "Sync failed: Connect to this host once from a tab to trust its host key, \
             then sync again."
        )
    );
    drop(page);
    profiles_window.destroy();
    main.window.destroy();
    pump_for(Duration::from_millis(100));
}

/// How many main windows `app` has.
fn main_windows(app: &adw::Application) -> usize {
    app.windows()
        .iter()
        .filter(|w| w.widget_name() == "moniker-main")
        .count()
}

fn close_all(app: &adw::Application) {
    for w in app.windows() {
        w.destroy();
    }
    pump_for(Duration::from_millis(100));
}

pub(super) fn auto_sync_starts_after_the_window_is_shown(dir: &Path) {
    let (ctx, _secrets, profile) = sync_ctx(dir, true, Some(SYNC_KEY));
    set_partner(&ctx, &profile.expect("a profile"), true);
    let app = test_app();
    // Made on the main thread as the sync starts: was a main window shown?
    let shown: Rc<RefCell<Vec<bool>>> = Rc::default();
    *ctx.sync_runner.borrow_mut() = Some(Box::new({
        let app = app.downgrade();
        let shown = Rc::clone(&shown);
        move || {
            let visible = app.upgrade().is_some_and(|app| {
                window::last_active(&app)
                    .is_some_and(|main| main.window.is_visible() && main.window.is_mapped())
            });
            shown.borrow_mut().push(visible);
            Box::new(|_| Ok(added(1)))
        }
    }));
    let delivered = ctx.notice.delivered();

    crate::launch(&app, &ctx);
    assert_eq!(main_windows(&app), 1, "the first window");
    assert!(
        shown.borrow().is_empty(),
        "nothing syncs before it is shown"
    );
    assert!(!ctx.sync_running.get());

    pump_until("the automatic sync to start", || !shown.borrow().is_empty());
    assert_eq!(
        *shown.borrow(),
        [true],
        "it started after the window was shown"
    );
    pump_until("the automatic sync to finish", || !ctx.sync_running.get());
    pump_for(Duration::from_millis(100));
    assert_eq!(ctx.notice.delivered(), delivered, "a success is silent");
    close_all(&app);
}

pub(super) fn auto_sync_failure_is_one_notification(dir: &Path) {
    let (ctx, secrets, profile) = sync_ctx(dir, true, Some(SYNC_KEY));
    set_partner(&ctx, &profile.expect("a profile"), true);
    let app = test_app();
    let runs = Arc::new(AtomicUsize::new(0));

    // The exchange fails: one notification.
    set_runner(&ctx, counting(&runs, || Err(SyncError::WrongSyncKey)));
    let delivered = ctx.notice.delivered();
    crate::launch(&app, &ctx);
    pump_until("the automatic sync", || runs.load(Ordering::SeqCst) == 1);
    pump_until("it to finish", || !ctx.sync_running.get());
    pump_for(Duration::from_millis(300));
    assert_eq!(
        ctx.notice.delivered(),
        delivered + 1,
        "exactly one notification"
    );
    assert_eq!(
        ctx.notice.last_text().as_deref(),
        Some("Automatic sync failed: The other machine has a different sync key.")
    );
    close_all(&app);

    // It cannot even start (no sync key, or a keyring that would not open):
    // one notification too, and nothing runs.
    secrets.delete(SYNC_PASSPHRASE_KEY).unwrap();
    set_runner(&ctx, counting(&runs, || Ok(added(1))));
    let delivered = ctx.notice.delivered();
    crate::launch(&app, &ctx);
    // Wait for the notification itself, not a fixed time: the launch sync
    // starts from an idle callback after the window, which a slow machine
    // reaches late. Then a little longer, to see that nothing else comes.
    pump_until("the notification", || ctx.notice.delivered() > delivered);
    pump_for(Duration::from_millis(300));
    assert_eq!(runs.load(Ordering::SeqCst), 1, "nothing ran");
    assert_eq!(
        ctx.notice.delivered(),
        delivered + 1,
        "exactly one notification"
    );
    assert_eq!(
        ctx.notice.last_text().as_deref(),
        Some("Automatic sync failed: Set a sync key first.")
    );
    close_all(&app);

    // A success: no notification at all.
    secrets.set(SYNC_PASSPHRASE_KEY, SYNC_KEY).unwrap();
    set_runner(&ctx, counting(&runs, || Ok(added(1))));
    let delivered = ctx.notice.delivered();
    crate::launch(&app, &ctx);
    pump_until("the automatic sync", || runs.load(Ordering::SeqCst) == 2);
    pump_until("it to finish", || !ctx.sync_running.get());
    pump_for(Duration::from_millis(300));
    assert_eq!(ctx.notice.delivered(), delivered, "a success is silent");
    close_all(&app);
}

pub(super) fn switch_off_stops_auto_sync(dir: &Path) {
    let (ctx, _secrets, profile) = sync_ctx(dir, true, Some(SYNC_KEY));
    let profile = profile.expect("a profile");
    set_partner(&ctx, &profile, true);
    let app = test_app();
    crate::add_actions(&app, &ctx);
    let window = open_sync_page(&app, &ctx);
    let page = page_in(&window);
    assert!(page.auto_sync.is_active());
    assert_eq!(
        page.auto_sync.subtitle().as_deref(),
        Some(sync_page::auto_sync_with(NAME).as_str())
    );
    assert_eq!(
        page.chosen().as_deref(),
        Some(NAME),
        "the partner is chosen"
    );

    // Off: saved at once, the partner kept.
    page.auto_sync.set_active(false);
    assert_eq!(
        settings::load(&ctx.settings_path),
        GuiSettings {
            sync_partner: Some(profile.id.clone()),
            auto_sync: false,
            ..GuiSettings::default()
        }
    );
    let made = Rc::new(Cell::new(0));
    let count_syncs = |ctx: &AppContext| {
        let made = Rc::clone(&made);
        *ctx.sync_runner.borrow_mut() = Some(Box::new(move || {
            made.set(made.get() + 1);
            Box::new(|_| Ok(added(0)))
        }));
    };
    count_syncs(&ctx);
    crate::launch(&app, &ctx);
    pump_for(Duration::from_millis(400));
    assert_eq!(made.get(), 0, "no sync on launch");
    assert!(!ctx.sync_running.get());
    assert!(sync_now_enabled(&app), "Sync Now stays: there is a partner");

    // A sync by hand does not turn it back on.
    page.sync_button.emit_clicked();
    pump_until("the sync", || made.get() == 1 && !ctx.sync_running.get());
    assert!(!settings::load(&ctx.settings_path).auto_sync);
    assert!(!page.auto_sync.is_active());

    // On again: the next launch syncs.
    page.auto_sync.set_active(true);
    assert!(settings::load(&ctx.settings_path).auto_sync);
    crate::launch(&app, &ctx);
    pump_until("the sync on launch", || made.get() == 2);
    pump_until("it to finish", || !ctx.sync_running.get());
    drop(page);
    close_all(&app);
}

pub(super) fn a_sync_refreshes_the_sidebar_and_new_tab_list(dir: &Path) {
    let (ctx, _secrets, profile) = sync_ctx(dir, true, Some(SYNC_KEY));
    set_partner(&ctx, &profile.expect("a profile"), true);
    let app = test_app();
    crate::add_actions(&app, &ctx);
    let profiles_window = profiles::present(&app, &ctx, Rc::new(|_: &Profile, _, _| {}));
    let main = window::build(&app, &ctx);
    main.window.present();
    pump_for(Duration::from_millis(300));
    let sidebar_titles = || -> Vec<String> {
        let list = widgets_of_type(&profiles_window, "GtkListBox")
            .into_iter()
            .filter_map(|w| w.downcast::<gtk::ListBox>().ok())
            .find(|l| l.widget_name() == profiles::SIDEBAR_NAME)
            .expect("the sidebar");
        let mut titles = Vec::new();
        let mut index = 0;
        while let Some(row) = list.row_at_index(index) {
            let row: adw::ActionRow = row.downcast().expect("an action row");
            titles.push(row.title().to_string());
            index += 1;
        }
        titles.sort();
        titles
    };
    assert_eq!(sidebar_titles(), [NAME]);

    // A new-tab list is open while the sync runs, with a search typed in it.
    let page = new_tab::open(&main.view, &ctx);
    pump_for(Duration::from_millis(300));
    let list_titles = |page: &adw::TabPage| -> Vec<String> {
        let list = all_widgets::<gtk::ListBox>(&page.child())
            .into_iter()
            .find(|l| l.widget_name() == new_tab::LIST_NAME)
            .expect("the new-tab list");
        let mut titles = Vec::new();
        let mut index = 0;
        while let Some(row) = list.row_at_index(index) {
            if row.is_child_visible() {
                let row: adw::ActionRow = row.downcast().expect("an action row");
                titles.push(row.title().to_string());
            }
            index += 1;
        }
        titles
    };
    let search: gtk::SearchEntry = find_widget(&page.child()).expect("the search entry");
    search.set_text("alp");
    pump_for(Duration::from_millis(300));
    assert_eq!(
        list_titles(&page),
        [new_tab::LOCAL_LABEL],
        "Alpha is not there yet"
    );

    // The worker adds a profile to the file, as a real sync would.
    set_runner(&ctx, |inputs: SyncInputs| {
        let mut store = ProfileStore::open(&inputs.store_path).expect("open");
        store
            .upsert(Profile::new("Alpha", "alpha.example.test", "bob"))
            .expect("upsert");
        Ok(added(1))
    });
    app.activate_action("sync-now", None);
    pump_until("the sync to finish", || !ctx.sync_running.get());
    pump_for(Duration::from_millis(100));
    assert_eq!(sidebar_titles(), [NAME, "Alpha"], "the sidebar lists it");
    assert_eq!(
        list_titles(&page),
        [new_tab::LOCAL_LABEL, "Alpha"],
        "the open new-tab list lists it, under the search still typed in"
    );
    assert_eq!(search.text(), "alp");
    // And so does the next one.
    let next = new_tab::open(&main.view, &ctx);
    pump_for(Duration::from_millis(300));
    assert!(list_titles(&next).contains(&"Alpha".to_owned()));
    profiles_window.destroy();
    main.window.destroy();
    pump_for(Duration::from_millis(100));
}

/// Every way a sync starts (Sync Now, the sync page, the sync on launch)
/// reads the keyring on the worker only: the main thread, which runs every
/// terminal, never waits for a locked keyring. The sync still gets the key.
pub(super) fn syncs_read_the_keyring_on_the_worker_only(dir: &Path) {
    let reads = Reads::default();
    let (ctx, secrets, profile) = sync_ctx_counting(dir, true, Some(SYNC_KEY), &reads);
    let profile = profile.expect("a profile");
    secrets
        .set(&password_key(&profile.id), "pw-for-ssh")
        .unwrap();
    set_partner(&ctx, &profile, true);
    let app = test_app();
    crate::add_actions(&app, &ctx);
    let main = window::build(&app, &ctx);
    main.window.present();
    // Opening the page shows whether a key is set: that reads the keyring,
    // as the page always has. Count from here.
    let profiles_window = open_sync_page(&app, &ctx);
    let page = page_in(&profiles_window);
    let main_reads = reads.main.load(Ordering::SeqCst);

    let check = |what: &str, start: &dyn Fn()| {
        let worker_reads = reads.worker.load(Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        set_runner(
            &ctx,
            fake_runner(tx, Arc::new(AtomicUsize::new(0)), Duration::ZERO, || {
                Ok(added(0))
            }),
        );
        start();
        pump_until(what, || {
            rx.try_recv().is_ok_and(|seen| {
                assert_eq!(seen.passphrase, SYNC_KEY, "{what}: the worker read the key");
                assert!(seen.has_password, "{what}: and the stored-password flag");
                true
            })
        });
        pump_until("the sync to finish", || !ctx.sync_running.get());
        assert_eq!(
            reads.main.load(Ordering::SeqCst),
            main_reads,
            "{what}: no keyring read on the main thread"
        );
        assert!(
            reads.worker.load(Ordering::SeqCst) > worker_reads,
            "{what}: the worker read it"
        );
    };
    check("Sync Now", &|| app.activate_action("sync-now", None));
    check("the sync page", &|| page.sync_button.emit_clicked());
    check("the sync on launch", &|| crate::launch(&app, &ctx));
    drop(page);
    close_all(&app);
}
