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
use moniker_core::store::{ProfileStore, SyncSwitch};
use moniker_core::sync::{StaleSync, SyncError, SyncOutcome};
use zeroize::Zeroizing;

use super::main_window::test_app;
use super::{
    HeldKeyring, HeldProfileFile, WAIT, alert_in, all_widgets, assert_plain_row, button_labeled,
    find_widget, has_focus_in, labels_in, make_helper, pump_for, pump_until, row_for,
    widgets_of_type,
};
use crate::context::AppContext;
use crate::launch_args::{Run, Start, encode};
use crate::profiles::{self, sync_page};
use crate::settings::{self, GuiSettings};
use crate::sync::{self, Runner, SyncInputs};
use crate::window::{self, MainWindow};
use crate::{keyring, new_tab};

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
        Arc::new(Shared {
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
    /// The page's own switch for sync (issue #10), first on the page.
    sync_switch: adw::SwitchRow,
    machine: adw::ComboRow,
    auto_sync: adw::SwitchRow,
    /// The sync partner (issue #11), shown while there is one, its Forget
    /// button, and the line a forget leaves.
    partner_row: adw::ActionRow,
    forget_button: gtk::Button,
    partner_message: gtk::Label,
    hint: gtk::Label,
    sync_button: gtk::Button,
    result: gtk::Label,
}

/// The switch row titled `title` on the sync page (there are two).
fn switch_titled(root: &impl IsA<gtk::Widget>, title: &str) -> adw::SwitchRow {
    all_widgets::<adw::SwitchRow>(root)
        .into_iter()
        .find(|row| row.title() == title)
        .unwrap_or_else(|| panic!("no switch titled {title}"))
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
        sync_switch: switch_titled(&widget, "Sync"),
        machine: find_widget(&widget).expect("the profile choice"),
        auto_sync: switch_titled(&widget, "Sync Automatically"),
        partner_row: row_for(&widget, sync_page::PARTNER_ROW_NAME),
        forget_button: button_labeled(&widget, "Forget").expect("the Forget button"),
        partner_message: named_label(&widget, sync_page::PARTNER_MESSAGE_NAME),
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

    /// Press Forget and wait for its question (issue #11).
    fn press_forget(&self, window: &adw::Window) -> adw::AlertDialog {
        self.forget_button.emit_clicked();
        alert_in(window)
    }
}

/// Answer `alert` with its button labeled `label`, and wait for it to go.
fn answer(window: &adw::Window, alert: &adw::AlertDialog, label: &str) {
    button_labeled(alert, label)
        .unwrap_or_else(|| panic!("the {label} button"))
        .emit_clicked();
    pump_until("the question to go", || {
        all_widgets::<adw::AlertDialog>(window).is_empty()
    });
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
    pump_until("the page to read the key's state", || {
        page.key_status.label() != sync::KEY_CHECKING
    });
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
    let window = profiles::present_sync(app, ctx, Rc::new(|_, _| {}));
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

/// Whether Sync Now can be chosen: its action exists and is enabled.
fn sync_now_enabled(app: &adw::Application) -> bool {
    app.lookup_action("sync-now")
        .is_some_and(|action| action.is_enabled())
}

/// Whether the `app.sync-now` action exists. Without it the ☰ menu's item is
/// hidden (`hidden-when: action-missing`); with it disabled the item shows
/// grayed out.
fn sync_now_exists(app: &adw::Application) -> bool {
    app.lookup_action("sync-now").is_some()
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
    pump_until("the key to be saved", || {
        d.key_message.label() == "Sync key saved."
    });
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
    pump_until("the status line to catch up", || {
        d.key_status.label() == "Sync key: not set"
    });

    // Saving stores it exactly as typed and clears the field.
    d.key_entry.set_text(SYNC_KEY);
    d.key_entry.emit_by_name::<()>("apply", &[]);
    pump_until("the key to be saved and shown as set", || {
        d.key_message.label() == "Sync key saved." && d.key_status.label() == "Sync key: set"
    });
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
    pump_until("the key to be replaced", || {
        rig.secrets
            .get(SYNC_PASSPHRASE_KEY)
            .unwrap()
            .is_some_and(|key| key.as_str() == "second sync key")
            && d.key_entry.is_sensitive()
    });
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
    // catches up. (Removed through a handle of its own, as a sync elsewhere
    // does; the window's snapshot is read again without telling anyone.)
    ProfileStore::open(&path)
        .unwrap()
        .delete(&profile.id)
        .unwrap();
    rig.ctx.store.borrow_mut().reload().unwrap();
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
        Some(("Sync Now".to_owned(), "action-missing".to_owned())),
        "the menu item hides while its action is missing"
    );
    assert!(!sync_now_exists(&rig.app), "no partner: no Sync Now");
    assert_eq!(
        rig.page.auto_sync.subtitle().as_deref(),
        Some(sync_page::AUTO_SYNC_NO_PARTNER)
    );
    assert!(rig.page.auto_sync.is_active(), "on by default");

    // A failed sync records nothing.
    rig.page
        .sync_with(&rig.ctx, |_| Err(SyncError::WrongSyncKey));
    pump_until("the failed sync", || rig.page.sync_button.is_sensitive());
    assert!(!sync_now_exists(&rig.app));
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
    let path = rig.ctx.store.borrow().path().to_path_buf();
    ProfileStore::open(&path)
        .unwrap()
        .delete(&profile.id)
        .unwrap();
    rig.ctx.profiles_changed();
    assert!(
        !sync_now_exists(&rig.app),
        "the partner is gone: Sync Now goes away"
    );
    main.window.destroy();
    rig.finish();
}

pub(super) fn sync_now_follows_a_forgotten_partner(dir: &Path) {
    let rig = sync_rig(dir, true, Some(SYNC_KEY));
    let profile = rig.profile.clone().expect("a profile");
    crate::add_actions(&rig.app, &rig.ctx);
    let main = window::build(&rig.app, &rig.ctx);
    main.window.present();
    pump_for(Duration::from_millis(200));

    // A partner, as a sync from the page records it: Sync Now is offered, and
    // the launch has a target.
    set_partner(&rig.ctx, &profile, true);
    rig.ctx.profiles_changed();
    assert!(sync_now_enabled(&rig.app), "Sync Now is offered");
    assert_eq!(
        sync::auto_sync_target(
            &settings::load(&rig.ctx.settings_path),
            &rig.ctx.store.borrow()
        ),
        Some(profile.id.clone()),
        "the launch syncs with it"
    );

    // Forgotten: no profile changed and no sync ended, only the partner
    // setting, and Sync Now goes at once.
    sync::forget_partner(&rig.ctx.settings_path).unwrap();
    rig.ctx.partner_changed();
    assert!(!sync_now_exists(&rig.app), "no partner: no Sync Now");
    assert_eq!(
        sync::auto_sync_target(
            &settings::load(&rig.ctx.settings_path),
            &rig.ctx.store.borrow()
        ),
        None,
        "the next launch does not sync"
    );
    main.window.destroy();
    rig.finish();
}

// ----- forgetting the partner (issue #11) -----

pub(super) fn the_sync_page_names_and_forgets_the_partner(dir: &Path) {
    let rig = sync_rig(dir, true, Some(SYNC_KEY));
    let profile = rig.profile.clone().expect("a profile");
    let (ctx, page) = (&rig.ctx, &rig.page);
    crate::add_actions(&rig.app, ctx);
    let main = window::build(&rig.app, ctx);
    main.window.present();
    pump_for(Duration::from_millis(200));

    // No partner: no row.
    assert!(!page.partner_row.is_visible());

    // A sync from the page that succeeds records the partner, and the row
    // names it, as plain text.
    page.sync_with(ctx, |_| Ok(added(0)));
    pump_until("the sync", || page.sync_button.is_sensitive());
    assert!(page.partner_row.is_visible());
    assert_plain_row("partner row", &page.partner_row, "Sync Partner", Some(NAME));
    assert_eq!(
        page.forget_button.tooltip_text().as_deref(),
        Some("Forget the sync partner and the sync key")
    );
    // The question it asks has the destructive answer, not the button.
    assert!(!page.forget_button.has_css_class("destructive-action"));

    // The profile renamed elsewhere: the row shows the new name.
    let path = ctx.store.borrow().path().to_path_buf();
    let rename = |name: &str| {
        let mut other = ProfileStore::open(&path).unwrap();
        let mut renamed = other.get(&profile.id).expect("the profile").clone();
        renamed.name = name.to_owned();
        other.upsert(renamed).unwrap();
        ctx.profiles_changed();
    };
    rename("<i>renamed</i> & _y_");
    assert_plain_row(
        "renamed partner row",
        &page.partner_row,
        "Sync Partner",
        Some("<i>renamed</i> & _y_"),
    );
    rename(NAME);
    assert_plain_row("partner row", &page.partner_row, "Sync Partner", Some(NAME));

    // A settings file with other settings in it (a hand edit, say), and a
    // profile's stored password, which a forget must leave alone.
    let before = GuiSettings {
        sync_partner: Some(profile.id.clone()),
        auto_sync: false,
        font_scale: 1.5,
        show_sessions: false,
        ..GuiSettings::default()
    };
    settings::save(&ctx.settings_path, &before).expect("save the settings");
    ctx.profiles_changed();
    rig.secrets
        .set(&password_key(&profile.id), "hunter2")
        .expect("store a password");
    let profile_file = fs::read(&path).expect("the profile file");

    // Forget asks first, naming the partner as plain text. The keyboard is on
    // Forget when it is pressed.
    page.forget_button.grab_focus();
    let alert = page.press_forget(&rig.window);
    assert_eq!(alert.heading().as_deref(), Some(sync_page::FORGET_HEADING));
    assert!(!alert.is_heading_use_markup());
    assert_eq!(alert.body().as_str(), sync_page::forget_body(NAME));
    assert!(alert.body().contains("remove the sync key"));
    assert!(!alert.is_body_use_markup());
    assert_eq!(alert.default_response().as_deref(), Some("cancel"));
    assert_eq!(alert.close_response().as_str(), "cancel");
    assert_eq!(alert.response_label("cancel").as_str(), "Cancel");
    assert_eq!(alert.response_label("forget").as_str(), "Forget");
    assert_eq!(
        alert.response_appearance("forget"),
        adw::ResponseAppearance::Destructive
    );
    // A second click (a double click) asks no second question.
    page.forget_button.emit_clicked();
    pump_for(Duration::from_millis(100));
    assert_eq!(
        all_widgets::<adw::AlertDialog>(&rig.window).len(),
        1,
        "one question"
    );

    // Cancel, and closing the question (Escape), change nothing, and the
    // keyboard comes back to Forget.
    let unchanged = |what: &str| {
        assert_eq!(
            settings::load(&ctx.settings_path),
            before,
            "{what}: the settings"
        );
        assert!(
            rig.secrets.get(SYNC_PASSPHRASE_KEY).unwrap().is_some(),
            "{what}: the key"
        );
        assert!(page.partner_row.is_visible(), "{what}: the row");
        assert!(sync_now_exists(&rig.app), "{what}: Sync Now");
        assert!(!page.partner_message.is_visible(), "{what}: no line");
    };
    answer(&rig.window, &alert, "Cancel");
    pump_until("the keyboard back on Forget after Cancel", || {
        has_focus_in(&rig.window, &page.forget_button)
    });
    unchanged("Cancel");
    let alert = page.press_forget(&rig.window);
    alert.close();
    pump_until("the question to go", || {
        all_widgets::<adw::AlertDialog>(&rig.window).is_empty()
    });
    pump_until("the keyboard back on Forget after Escape", || {
        has_focus_in(&rig.window, &page.forget_button)
    });
    unchanged("Escape");

    // Forget: the sync key goes, then the partner, and nothing else.
    let alert = page.press_forget(&rig.window);
    answer(&rig.window, &alert, "Forget");
    pump_until("the forget", || !page.partner_row.is_visible());
    assert!(
        rig.secrets.get(SYNC_PASSPHRASE_KEY).unwrap().is_none(),
        "the sync key is removed"
    );
    assert_eq!(
        rig.secrets
            .get(&password_key(&profile.id))
            .unwrap()
            .expect("the password")
            .as_str(),
        "hunter2",
        "the password stays"
    );
    assert_eq!(page.key_status.label(), "Sync key: not set");
    assert_eq!(
        settings::load(&ctx.settings_path),
        GuiSettings {
            sync_partner: None,
            ..before
        }
    );
    assert!(!sync_now_exists(&rig.app), "no partner: no Sync Now");
    assert!(!page.auto_sync.is_active(), "the switch keeps its value");
    assert_eq!(
        page.auto_sync.subtitle().as_deref(),
        Some(sync_page::AUTO_SYNC_NO_PARTNER)
    );
    // The page says what was done, in its ordinary style.
    assert!(page.partner_message.is_visible());
    assert_eq!(page.partner_message.label(), sync_page::forgotten(NAME));
    assert!(!page.partner_message.uses_markup());
    assert!(!has_css(&page.partner_message, "error"));
    // With no key, Sync and the Profile row wait for one, so the keyboard goes
    // to the Sync switch, never to the Sync button: Enter pressed again there
    // would sync with the machine just forgotten.
    assert_eq!(page.hint.label(), "Set a sync key first.");
    assert!(!page.sync_button.is_sensitive());
    assert!(!page.machine.is_sensitive());
    pump_until("the keyboard on the Sync switch", || {
        has_focus_in(&rig.window, &page.sync_switch)
    });
    assert_eq!(fs::read(&path).unwrap(), profile_file, "the profiles");

    // A new key hides the line, which says what is wanted until then.
    page.key_entry.set_text(SYNC_KEY);
    page.key_entry.emit_by_name::<()>("apply", &[]);
    pump_until("the key to be saved", || {
        page.key_message.label() == "Sync key saved."
    });
    assert!(!page.partner_message.is_visible(), "a key is set again");
    pump_until("Sync to be usable", || page.sync_button.is_sensitive());

    // A sync starting hides the line too, so it never sits beside a newer
    // partner (marked here, as the new key hid it); the sync's success records
    // the partner again.
    page.partner_message.set_label("marked");
    page.partner_message.set_visible(true);
    let (tx, rx) = mpsc::channel();
    page.sync_with(
        ctx,
        fake_runner(
            tx,
            Arc::new(AtomicUsize::new(0)),
            Duration::from_millis(300),
            || Ok(added(0)),
        ),
    );
    rx.recv_timeout(WAIT).expect("the worker started");
    assert!(!page.partner_message.is_visible(), "hidden while it runs");
    pump_until("the sync", || page.sync_button.is_sensitive());
    assert!(!page.partner_message.is_visible());
    assert!(page.partner_row.is_visible(), "the row is back");
    assert!(sync_now_exists(&rig.app), "and Sync Now");

    // The partner's profile deleted: no partner to show.
    ProfileStore::open(&path)
        .unwrap()
        .delete(&profile.id)
        .unwrap();
    ctx.profiles_changed();
    assert!(!page.partner_row.is_visible());
    main.window.destroy();
    rig.finish();
}

pub(super) fn forget_waits_for_a_sync_but_not_for_sync_off(dir: &Path) {
    let rig = sync_rig(dir, true, Some(SYNC_KEY));
    let profile = rig.profile.clone().expect("a profile");
    let (ctx, page) = (&rig.ctx, &rig.page);
    set_partner(ctx, &profile, true);
    ctx.profiles_changed();
    assert!(page.partner_row.is_visible());
    assert!(page.forget_button.is_sensitive());

    // A sync would record the partner again right after: Forget waits.
    let (tx, rx) = mpsc::channel();
    page.sync_with(
        ctx,
        fake_runner(
            tx,
            Arc::new(AtomicUsize::new(0)),
            Duration::from_millis(300),
            || Ok(added(0)),
        ),
    );
    rx.recv_timeout(WAIT).expect("the worker started");
    assert!(!page.forget_button.is_sensitive(), "a sync runs");
    // A click that gets through anyway (the guard in `forget`) asks nothing
    // and forgets nothing.
    page.forget_button.emit_clicked();
    pump_for(Duration::from_millis(100));
    assert!(
        all_widgets::<adw::AlertDialog>(&rig.window).is_empty(),
        "no question during a sync"
    );
    assert!(
        settings::load(&ctx.settings_path).sync_partner.is_some(),
        "no forget during a sync"
    );
    assert!(rig.secrets.get(SYNC_PASSPHRASE_KEY).unwrap().is_some());
    assert!(!page.partner_message.is_visible());
    pump_until("the sync", || page.sync_button.is_sensitive());
    assert!(page.forget_button.is_sensitive());

    // Sync turned off (elsewhere): Sync is grayed out, Forget is not, since a
    // forget sends nothing.
    let path = ctx.store.borrow().path().to_path_buf();
    ProfileStore::open(&path).unwrap().set_sync(false).unwrap();
    ctx.profiles_changed();
    assert!(!page.sync_switch.is_active());
    assert!(!page.sync_button.is_sensitive());
    assert!(page.machine.is_sensitive());
    assert!(page.forget_button.is_sensitive());

    // It goes ahead. With the key gone the Profile row waits for one, so the
    // keyboard goes to the Sync switch.
    page.forget_button.grab_focus();
    let alert = page.press_forget(&rig.window);
    answer(&rig.window, &alert, "Forget");
    pump_until("the forget", || !page.partner_row.is_visible());
    assert_eq!(settings::load(&ctx.settings_path).sync_partner, None);
    assert!(rig.secrets.get(SYNC_PASSPHRASE_KEY).unwrap().is_none());
    pump_until("the keyboard on the Sync switch", || {
        has_focus_in(&rig.window, &page.sync_switch)
    });
    rig.finish();
}

pub(super) fn forget_leaves_a_bad_settings_file_and_says_so(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    const SAYS: &str = "Could not save the sync settings: ";
    let rig = sync_rig(dir, true, Some(SYNC_KEY));
    let profile = rig.profile.clone().expect("a profile");
    let (ctx, page) = (&rig.ctx, &rig.page);
    // Sync Now follows the settings file too.
    crate::add_actions(&rig.app, ctx);
    let area = super::bare_area();
    ctx.notice.attach(&area);
    let file = ctx.settings_path.clone();
    let told = || {
        ctx.notice
            .last_text()
            .is_some_and(|text| text.starts_with(SAYS))
    };
    // The key goes first, whatever the settings file then does: each case
    // puts it back.
    let key_back = || {
        rig.secrets
            .set(SYNC_PASSPHRASE_KEY, SYNC_KEY)
            .expect("the key again");
    };
    let removed = || rig.secrets.get(SYNC_PASSPHRASE_KEY).unwrap().is_none();

    // (a) A file that cannot be parsed is left as it is, and a notification
    // says so; the line says that only the key went. Moniker reads such a file
    // as the defaults, so everything that shows the partner then shows none, as
    // a page opened now would. (The switch is off in the file before, and the
    // defaults have it on: its change must not be saved over the damaged file.)
    set_partner(ctx, &profile, false);
    ctx.profiles_changed();
    assert!(page.partner_row.is_visible());
    assert!(sync_now_exists(&rig.app));
    assert!(!page.auto_sync.is_active());
    fs::write(&file, b"{ not json").expect("damage the file");
    let delivered = ctx.notice.delivered();
    page.forget_button.grab_focus();
    let alert = page.press_forget(&rig.window);
    // The question names what the row shows.
    assert_eq!(alert.body().as_str(), sync_page::forget_body(NAME));
    answer(&rig.window, &alert, "Forget");
    pump_until("the forget", || ctx.notice.delivered() > delivered);
    assert!(removed(), "the key went first");
    assert_eq!(fs::read(&file).unwrap(), b"{ not json", "left as it was");
    assert_eq!(ctx.notice.delivered(), delivered + 1);
    assert!(told(), "{:?}", ctx.notice.last_text());
    assert!(page.partner_message.is_visible());
    assert_eq!(page.partner_message.label(), sync_page::KEY_REMOVED);
    assert!(!page.partner_row.is_visible(), "the file names no partner");
    assert_eq!(
        page.auto_sync.subtitle().as_deref(),
        Some(sync_page::AUTO_SYNC_NO_PARTNER)
    );
    assert!(!sync_now_exists(&rig.app), "and Sync Now goes");
    assert!(page.auto_sync.is_active(), "the defaults, not saved");
    assert_eq!(fs::read(&file).unwrap(), b"{ not json", "still as it was");
    // The row went while it had the keyboard, and with no key the Profile row
    // waits for one: the Sync switch has it.
    pump_until("the keyboard on the Sync switch", || {
        has_focus_in(&rig.window, &page.sync_switch)
    });

    // (b) A folder that cannot be written to: the key goes, the settings write
    // fails, the row stays and the keyboard comes back to Forget.
    key_back();
    set_partner(ctx, &profile, true);
    ctx.profiles_changed();
    assert!(page.partner_row.is_visible());
    assert!(sync_now_exists(&rig.app));
    let before = fs::read(&file).unwrap();
    let delivered = ctx.notice.delivered();
    page.forget_button.grab_focus();
    let alert = page.press_forget(&rig.window);
    fs::set_permissions(dir, fs::Permissions::from_mode(0o500)).expect("chmod");
    answer(&rig.window, &alert, "Forget");
    pump_until("the forget", || ctx.notice.delivered() > delivered);
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).expect("chmod back");
    assert!(removed(), "the key went");
    assert_eq!(ctx.notice.delivered(), delivered + 1);
    assert!(told(), "{:?}", ctx.notice.last_text());
    assert!(page.partner_row.is_visible(), "the row stays");
    assert!(
        sync_now_exists(&rig.app),
        "the file can be read: the partner stays"
    );
    assert_eq!(page.partner_message.label(), sync_page::KEY_REMOVED);
    assert_eq!(fs::read(&file).unwrap(), before, "the file is unchanged");
    pump_until("the keyboard back on Forget", || {
        has_focus_in(&rig.window, &page.forget_button)
    });

    // (c) A file with no partner any more (changed behind the page's back):
    // the key goes, nothing is written or told, and the row catches up.
    key_back();
    set_partner(ctx, &profile, true);
    ctx.profiles_changed();
    let text = br#"{"auto_sync": false}"#;
    fs::write(&file, text).expect("rewrite the file");
    let delivered = ctx.notice.delivered();
    let alert = page.press_forget(&rig.window);
    answer(&rig.window, &alert, "Forget");
    pump_until("the forget", || removed() && !page.partner_row.is_visible());
    assert_eq!(fs::read(&file).unwrap(), text, "unchanged");
    assert_eq!(page.partner_message.label(), sync_page::KEY_REMOVED);
    assert_eq!(ctx.notice.delivered(), delivered, "and nothing is told");
    rig.finish();
}

/// Forget removes the sync key on the keyring worker before it forgets the
/// partner. While that is out (an unlock prompt, say), Forget, Sync, the Profile
/// row and the key field wait, and the partner is still there. A keyring that
/// refuses changes nothing else: a notification says why, the row stays, the
/// keyring is not read again (a locked one would prompt again), the keyboard
/// comes back to Forget, and Forget can be pressed again. A page that goes
/// before the keyring is back does not cut the forget short, and a refusal is
/// still told.
pub(super) fn forget_waits_for_the_keyring(dir: &Path) {
    fs::create_dir_all(dir).expect("dir");
    let mut store = ProfileStore::open(&dir.join("profiles.json")).expect("open the store");
    let profile = Profile::new(NAME, "app.example.test", "alice");
    store.upsert(profile.clone()).expect("save the profile");
    let seeded = Arc::new(MemorySecretStore::default());
    seeded
        .set(SYNC_PASSPHRASE_KEY, SYNC_KEY)
        .expect("set the key");
    let reads = Arc::new(AtomicUsize::new(0));
    let keyring = Arc::new(HeldKeyring::new(Arc::new(Shared {
        store: Arc::clone(&seeded),
        gets: Arc::clone(&reads),
    })));
    let ctx = Rc::new(AppContext::with_parts(
        store,
        Arc::clone(&keyring) as Arc<dyn SecretStore>,
        make_helper(dir),
    ));
    let app = test_app();
    crate::add_actions(&app, &ctx);
    let area = super::bare_area();
    ctx.notice.attach(&area);
    set_partner(&ctx, &profile, true);
    let window = open_sync_page(&app, &ctx);
    let page = page_in(&window);
    pump_until("the key's state", || {
        page.key_status.label() == "Sync key: set"
    });
    assert!(page.partner_row.is_visible());

    // While the key is being removed, what would use it waits, and the
    // partner is still there. The keyboard has nowhere to be meanwhile (Forget
    // is grayed out), and finds its place once the removal is back.
    keyring.hold();
    page.forget_button.grab_focus();
    let alert = page.press_forget(&window);
    answer(&window, &alert, "Forget");
    pump_until("the forget to start", || !page.key_entry.is_sensitive());
    assert!(!page.forget_button.is_sensitive());
    assert!(!page.sync_button.is_sensitive());
    assert!(!page.machine.is_sensitive());
    assert!(page.partner_row.is_visible(), "the key goes first");
    assert!(settings::load(&ctx.settings_path).sync_partner.is_some());
    // A click that gets through anyway asks nothing.
    page.forget_button.emit_clicked();
    pump_for(Duration::from_millis(300));
    assert!(
        all_widgets::<adw::AlertDialog>(&window).is_empty(),
        "no second question"
    );
    keyring.release();
    pump_until("the forget", || !page.partner_row.is_visible());
    assert!(seeded.get(SYNC_PASSPHRASE_KEY).unwrap().is_none());
    assert_eq!(settings::load(&ctx.settings_path).sync_partner, None);
    assert!(!sync_now_exists(&app));
    assert!(page.key_entry.is_sensitive());
    assert_eq!(page.partner_message.label(), sync_page::forgotten(NAME));
    pump_until("the keyboard on the Sync switch", || {
        has_focus_in(&window, &page.sync_switch)
    });

    // A keyring that refuses (a locked one whose prompt was canceled), with
    // the answer coming back after the question has gone: nothing else
    // changes, the line about the last forget goes, and the keyring is not
    // read again.
    page.key_entry.set_text(SYNC_KEY);
    page.key_entry.emit_by_name::<()>("apply", &[]);
    pump_until("the key to be saved", || {
        page.key_message.label() == "Sync key saved."
    });
    pump_until("the key's state", || {
        page.key_status.label() == "Sync key: set"
    });
    set_partner(&ctx, &profile, true);
    ctx.profiles_changed();
    assert!(page.partner_row.is_visible());
    // Marked, as the new key hid the last forget's line: the new forget must
    // hide it.
    page.partner_message.set_label("marked");
    page.partner_message.set_visible(true);
    keyring.refuse_changes(true);
    keyring.hold();
    let delivered = ctx.notice.delivered();
    page.forget_button.grab_focus();
    let alert = page.press_forget(&window);
    answer(&window, &alert, "Forget");
    pump_until("the forget to start", || !page.key_entry.is_sensitive());
    pump_for(Duration::from_millis(300));
    let reads_before = reads.load(Ordering::SeqCst);
    keyring.release();
    pump_until("the refusal", || ctx.notice.delivered() > delivered);
    pump_for(Duration::from_millis(300));
    assert_eq!(
        reads.load(Ordering::SeqCst),
        reads_before,
        "no second read after a refusal"
    );
    let told = ctx.notice.last_text().unwrap_or_default();
    assert!(
        told.starts_with("Could not remove the sync key: "),
        "{told}"
    );
    assert!(told.contains("the keyring is locked"), "{told}");
    assert!(
        seeded.get(SYNC_PASSPHRASE_KEY).unwrap().is_some(),
        "the key stays"
    );
    assert!(
        settings::load(&ctx.settings_path).sync_partner.is_some(),
        "and the partner"
    );
    assert!(page.partner_row.is_visible());
    assert!(sync_now_exists(&app));
    assert!(!page.partner_message.is_visible());
    assert_eq!(page.key_status.label(), "Sync key: set", "still");
    assert!(page.machine.is_sensitive() && page.sync_button.is_sensitive());
    pump_until("the keyboard back on Forget", || {
        has_focus_in(&window, &page.forget_button)
    });
    assert!(page.forget_button.is_sensitive(), "it can be pressed again");
    assert!(page.key_entry.is_sensitive());
    keyring.refuse_changes(false);

    // The page gone before the keyring is back: the forget still finishes.
    keyring.hold();
    let alert = page.press_forget(&window);
    answer(&window, &alert, "Forget");
    pump_until("the forget to start", || !page.key_entry.is_sensitive());
    let weak = page.widget.downgrade();
    drop(page);
    window.close();
    drop(window);
    pump_until("the page to be freed", || weak.upgrade().is_none());
    keyring.release();
    pump_until("the forget to finish", || {
        settings::load(&ctx.settings_path).sync_partner.is_none()
    });
    assert!(seeded.get(SYNC_PASSPHRASE_KEY).unwrap().is_none());
    assert!(!sync_now_exists(&app));

    // And a refusal with the page gone is still told; the partner stays.
    seeded
        .set(SYNC_PASSPHRASE_KEY, SYNC_KEY)
        .expect("the key again");
    set_partner(&ctx, &profile, true);
    let window = open_sync_page(&app, &ctx);
    let page = page_in(&window);
    pump_until("the key's state", || {
        page.key_status.label() == "Sync key: set"
    });
    keyring.refuse_changes(true);
    keyring.hold();
    let delivered = ctx.notice.delivered();
    let alert = page.press_forget(&window);
    answer(&window, &alert, "Forget");
    pump_until("the forget to start", || !page.key_entry.is_sensitive());
    let weak = page.widget.downgrade();
    drop(page);
    window.close();
    drop(window);
    pump_until("the page to be freed", || weak.upgrade().is_none());
    keyring.release();
    pump_until("the refusal", || ctx.notice.delivered() > delivered);
    let told = ctx.notice.last_text().unwrap_or_default();
    assert!(
        told.starts_with("Could not remove the sync key: "),
        "{told}"
    );
    assert!(seeded.get(SYNC_PASSPHRASE_KEY).unwrap().is_some());
    assert!(settings::load(&ctx.settings_path).sync_partner.is_some());
    keyring.refuse_changes(false);
    pump_for(Duration::from_millis(100));
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

    super::launch_at_home(&app, &ctx);
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
    super::launch_at_home(&app, &ctx);
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
    super::launch_at_home(&app, &ctx);
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
    super::launch_at_home(&app, &ctx);
    pump_until("the automatic sync", || runs.load(Ordering::SeqCst) == 2);
    pump_until("it to finish", || !ctx.sync_running.get());
    pump_for(Duration::from_millis(300));
    assert_eq!(ctx.notice.delivered(), delivered, "a success is silent");
    close_all(&app);
}

/// Issue #15: the first command line Moniker handles is the launch (its first
/// window, and the automatic sync once that window is shown); one that comes
/// while Moniker runs opens a window and nothing else, whatever it asks for
/// (no second sync). A line that was a mistake is no launch at all: it opens
/// nothing, starts nothing, and the next line is still the first.
pub(super) fn a_command_line_launches_once_and_then_opens_windows(dir: &Path) {
    let (ctx, _secrets, profile) = sync_ctx(dir, true, Some(SYNC_KEY));
    set_partner(&ctx, &profile.expect("a profile"), true);
    let app = test_app();
    let folder = dir.join("My Files");
    fs::create_dir_all(&folder).expect("the folder");
    let runs = Arc::new(AtomicUsize::new(0));
    set_runner(&ctx, counting(&runs, || Ok(added(1))));
    let launched = Cell::new(false);
    let asking = |dir: &Path| {
        encode(&Start {
            dir: dir.to_path_buf(),
            title: None,
            run: Run::Shell,
            dir_refused: None,
        })
    };

    // A mistake first: no window, no sync, and still no launch.
    let mistake = vec!["--moniker-frob=1".to_owned()];
    assert_eq!(
        crate::handle_command_line(&app, &ctx, &launched, &mistake),
        (
            2,
            Some("moniker: unexpected handover: --moniker-frob=1\n".to_owned())
        )
    );
    assert!(!launched.get(), "a mistake is not the launch");
    assert_eq!(main_windows(&app), 0);
    pump_for(Duration::from_millis(300));
    assert_eq!(runs.load(Ordering::SeqCst), 0, "no sync for a mistake");

    // The first line is the launch: one window, then the automatic sync.
    assert_eq!(
        crate::handle_command_line(&app, &ctx, &launched, &asking(&folder)),
        (0, None)
    );
    assert!(launched.get());
    assert_eq!(main_windows(&app), 1, "the first window");
    pump_until("the automatic sync", || runs.load(Ordering::SeqCst) == 1);
    pump_until("it to finish", || !ctx.sync_running.get());

    // The next ones are windows, and nothing syncs again (a runner is ready:
    // a second sync would use it and show in the count).
    set_runner(&ctx, counting(&runs, || Ok(added(1))));
    for line in [asking(&folder), Vec::new()] {
        assert_eq!(
            crate::handle_command_line(&app, &ctx, &launched, &line),
            (0, None)
        );
    }
    assert_eq!(main_windows(&app), 3, "one window for each line");
    pump_for(Duration::from_millis(500));
    assert_eq!(runs.load(Ordering::SeqCst), 1, "only the launch syncs");
    close_all(&app);
}

/// Runs the application's own main loop (`run`, not the tests' pumping) over
/// `ctx`: the launch (`launch_at_home`), then `after`, which gets the
/// application and may close its windows. Returns whether the loop had to be
/// stopped after `give_up` instead of ending by itself, which is what an
/// application still held does. The loop ends by itself exactly when the last
/// window is gone and nothing holds the application.
fn run_a_launch(
    ctx: &Rc<AppContext>,
    give_up: Duration,
    after: impl FnOnce(&adw::Application) + 'static,
) -> bool {
    let app = adw::Application::builder()
        .application_id("org.moniker.GuiTest.LaunchHold")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    let timed_out = Rc::new(Cell::new(false));
    let after = RefCell::new(Some(after));
    app.connect_activate({
        let (ctx, timed_out) = (Rc::clone(ctx), Rc::clone(&timed_out));
        move |app| {
            super::launch_at_home(app, &ctx);
            glib::timeout_add_local_once(
                give_up,
                glib::clone!(
                    #[weak]
                    app,
                    #[strong]
                    timed_out,
                    move || {
                        timed_out.set(true);
                        app.quit();
                    }
                ),
            );
            if let Some(after) = after.borrow_mut().take() {
                after(app);
            }
        }
    });
    app.run_with_args(&["moniker"]);
    timed_out.get()
}

/// Closes every window of `app`, as a command that ends at once does to its
/// window.
fn close_every_window(app: &adw::Application) {
    for window in app.windows() {
        window.destroy();
    }
}

/// Issue #15: a launch whose command ends at once (`moniker -- true` with
/// Moniker closed, `x-terminal-emulator -e quick-command`) closes its only
/// window while the automatic sync of that launch is still to start or still
/// running. The application stays until that sync is over, so that it is not
/// cut off between the secrets it stores and the profile file it writes after
/// them, and goes when the sync ends, however it ends, and at once if there is
/// no sync to wait for. Run on the application's real main loop.
pub(super) fn the_launch_sync_holds_the_application_until_it_ends(dir: &Path) {
    // The only window closes at once and the sync is slow: the application
    // stays while it runs, with no window, and leaves when it is over.
    let (ctx, _secrets, profile) = sync_ctx(&dir.join("slow"), true, Some(SYNC_KEY));
    set_partner(&ctx, &profile.expect("a profile"), true);
    let (started, runs) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (release, wait) = mpsc::channel::<()>();
    set_runner(&ctx, {
        let (started, runs) = (Arc::clone(&started), Arc::clone(&runs));
        move |_| {
            started.fetch_add(1, Ordering::SeqCst);
            // Slow: it ends when the test lets it (or after a minute).
            let _ = wait.recv_timeout(Duration::from_secs(60));
            runs.fetch_add(1, Ordering::SeqCst);
            Ok(added(1))
        }
    });
    let released = Rc::new(Cell::new(false));
    let release = Rc::new(release);
    let timed_out = run_a_launch(&ctx, Duration::from_secs(30), {
        let (started, released, release) = (
            Arc::clone(&started),
            Rc::clone(&released),
            Rc::clone(&release),
        );
        move |app| {
            close_every_window(app);
            glib::timeout_add_local(Duration::from_millis(20), move || {
                if started.load(Ordering::SeqCst) == 0 {
                    return glib::ControlFlow::Continue;
                }
                // The sync is running and no window is left. Give a
                // wrongly released application time to quit (it does so at
                // once), then let the sync end.
                let (released, release) = (Rc::clone(&released), Rc::clone(&release));
                glib::timeout_add_local_once(Duration::from_millis(500), move || {
                    released.set(true);
                    let _ = release.send(());
                });
                glib::ControlFlow::Break
            });
        }
    });
    let _ = release.send(());
    assert!(!timed_out, "the application was never let go");
    assert_eq!(
        started.load(Ordering::SeqCst),
        1,
        "the sync of the launch started, though its window was already gone"
    );
    assert!(
        released.get(),
        "the application quit while its sync was still running"
    );
    assert_eq!(runs.load(Ordering::SeqCst), 1, "the sync ran to its end");
    assert!(!ctx.sync_running.get(), "and was told so");

    // A sync that fails lets go too.
    let (ctx, _secrets, profile) = sync_ctx(&dir.join("fails"), true, Some(SYNC_KEY));
    set_partner(&ctx, &profile.expect("a profile"), true);
    let runs = Arc::new(AtomicUsize::new(0));
    set_runner(&ctx, counting(&runs, || Err(SyncError::WrongSyncKey)));
    let timed_out = run_a_launch(&ctx, Duration::from_secs(20), close_every_window);
    assert!(
        !timed_out,
        "a failed sync did not let go of the application"
    );
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    assert!(!ctx.sync_running.get());

    // Nothing to sync (no partner is set up): no wait at all.
    let (ctx, _secrets, _profile) = sync_ctx(&dir.join("none"), true, Some(SYNC_KEY));
    let runs = Arc::new(AtomicUsize::new(0));
    set_runner(&ctx, counting(&runs, || Ok(added(1))));
    let timed_out = run_a_launch(&ctx, Duration::from_secs(20), close_every_window);
    assert!(!timed_out, "the launch held the application for no sync");
    assert_eq!(runs.load(Ordering::SeqCst), 0, "nothing synced");

    // A sync that cannot start because one is running already (started by
    // hand a moment ago): the launch's own wait is over at once as well.
    let (ctx, _secrets, profile) = sync_ctx(&dir.join("busy"), true, Some(SYNC_KEY));
    set_partner(&ctx, &profile.expect("a profile"), true);
    let runs = Arc::new(AtomicUsize::new(0));
    set_runner(&ctx, counting(&runs, || Ok(added(1))));
    ctx.sync_running.set(true);
    let timed_out = run_a_launch(&ctx, Duration::from_secs(20), close_every_window);
    assert!(!timed_out, "a sync that did not start held the application");
    assert_eq!(runs.load(Ordering::SeqCst), 0, "no second sync");
    ctx.sync_running.set(false);
}

/// Issue #10: while sync is turned off (as the command line or another Moniker
/// wrote it in the profile file) nothing starts a sync: not the launch, not Sync
/// Now, not the sync page's button, and the keyring is not even read for one.
/// Sync Now stays in the menu, grayed out, as long as there is a partner.
pub(super) fn sync_off_grays_out_sync_now_and_skips_launch(dir: &Path) {
    let reads = Reads::default();
    let (ctx, _secrets, profile) = sync_ctx_counting(dir, true, Some(SYNC_KEY), &reads);
    let profile = profile.expect("a profile");
    set_partner(&ctx, &profile, true);
    let path = ctx.store.borrow().path().to_path_buf();
    // Another handle, as `moniker-server sync-switch off` would write it.
    ProfileStore::open(&path).unwrap().set_sync(false).unwrap();
    ctx.profiles_changed();
    assert!(!ctx.store.borrow().sync_switch().is_on());

    let app = test_app();
    crate::add_actions(&app, &ctx);
    let runs = Arc::new(AtomicUsize::new(0));
    set_runner(&ctx, counting(&runs, || Ok(added(1))));
    let delivered = ctx.notice.delivered();

    // Launch: nothing runs, and the person, who turned it off on purpose, is
    // told nothing.
    super::launch_at_home(&app, &ctx);
    let main = window::last_active(&app).expect("the first window");
    pump_for(Duration::from_secs(1));
    assert_eq!(runs.load(Ordering::SeqCst), 0, "no sync on launch");
    assert!(!ctx.sync_running.get());
    assert_eq!(ctx.notice.delivered(), delivered, "and no notification");
    assert_eq!(
        (
            reads.main.load(Ordering::SeqCst),
            reads.worker.load(Ordering::SeqCst)
        ),
        (0, 0),
        "no keyring read for a sync that cannot run"
    );

    // Sync Now is in the menu, grayed out: its action exists and is disabled.
    assert_eq!(
        sync_now_item(&main),
        Some(("Sync Now".to_owned(), "action-missing".to_owned()))
    );
    assert!(sync_now_exists(&app), "shown");
    assert!(!sync_now_enabled(&app), "but grayed out");
    app.activate_action("sync-now", None);
    pump_for(Duration::from_millis(200));
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert_eq!(ctx.notice.delivered(), delivered);

    // The sync page says why Sync is unavailable. Even a press that gets past
    // the insensitive button (a test can) starts nothing: the request is
    // refused on the main thread, before any worker or keyring.
    let profiles_window = open_sync_page(&app, &ctx);
    let page = page_in(&profiles_window);
    pump_until("the page to read the key's state", || {
        page.key_status.label() != sync::KEY_CHECKING
    });
    assert_eq!(page.key_status.label(), "Sync key: set");
    assert!(page.hint.is_visible());
    assert_eq!(page.hint.label(), sync::SYNC_IS_OFF);
    assert!(!page.sync_button.is_sensitive());
    let worker_reads = reads.worker.load(Ordering::SeqCst);
    page.sync_button.emit_clicked();
    assert_eq!(page.result.label(), sync::SYNC_IS_OFF);
    assert!(!ctx.sync_running.get());
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert_eq!(reads.worker.load(Ordering::SeqCst), worker_reads);

    // The partner deleted (by a sync, say): Sync Now is hidden again, off or not.
    ProfileStore::open(&path)
        .unwrap()
        .delete(&profile.id)
        .unwrap();
    ctx.profiles_changed();
    assert!(!sync_now_exists(&app), "no partner: hidden");

    // Another partner, sync still off: shown, grayed out. The file watch is
    // what tells the menu in the app; here the reload is asked for.
    let other = Profile::new("other", "other.example.test", "bob");
    let other_id = other.id.clone();
    ProfileStore::open(&path).unwrap().upsert(other).unwrap();
    ctx.profiles_changed();
    let other = ctx.store.borrow().get(&other_id).expect("saved").clone();
    set_partner(&ctx, &other, true);
    ctx.profiles_changed();
    assert!(sync_now_exists(&app));
    assert!(!sync_now_enabled(&app), "still off: grayed out");

    // Turned on: Sync Now can be chosen, the page's hint goes, and the launch
    // syncs (the control for everything above).
    ProfileStore::open(&path).unwrap().set_sync(true).unwrap();
    ctx.profiles_changed();
    assert!(sync_now_enabled(&app), "on: enabled");
    assert!(!page.hint.is_visible(), "the page no longer says it is off");
    assert!(page.sync_button.is_sensitive());
    // (The press above asked for its runner before it was refused.)
    set_runner(&ctx, counting(&runs, || Ok(added(1))));
    super::launch_at_home(&app, &ctx);
    pump_until("the sync on launch", || runs.load(Ordering::SeqCst) == 1);
    pump_until("it to finish", || !ctx.sync_running.get());
    assert_eq!(ctx.notice.delivered(), delivered, "a success is silent");
    set_runner(&ctx, counting(&runs, || Ok(added(0))));
    app.activate_action("sync-now", None);
    pump_until("Sync Now to run", || runs.load(Ordering::SeqCst) == 2);
    pump_until("it to finish", || !ctx.sync_running.get());

    // Off again: grayed out again, nothing runs.
    ProfileStore::open(&path).unwrap().set_sync(false).unwrap();
    ctx.profiles_changed();
    assert!(sync_now_exists(&app));
    assert!(!sync_now_enabled(&app));
    set_runner(&ctx, counting(&runs, || Ok(added(0))));
    app.activate_action("sync-now", None);
    pump_for(Duration::from_millis(200));
    assert_eq!(runs.load(Ordering::SeqCst), 2, "no third sync");
    drop(page);
    close_all(&app);
}

/// What the profile file says about the sync switch, read through a handle of
/// its own (not the context's snapshot, which follows the file only after a
/// reload).
fn switch_on_disk(ctx: &AppContext) -> SyncSwitch {
    ProfileStore::open(ctx.store.borrow().path())
        .expect("open the store")
        .sync_switch()
}

/// The body of the "Turn Sync On?" alert.
const TURN_ON_BODY: &str = "At the next sync, every profile on this machine that syncs, \
    including any added while sync was off, will be copied to your other machines with its \
    stored password. To keep a profile on this machine only, cancel and turn off \"Sync to \
    other machines\" on its page first.";

/// Issue #10: the sync page's own switch. Turning it off is saved at once and
/// asks nothing; Sync Automatically and Sync go gray (the choice of the other
/// machine stays: choosing sends nothing) and the hint says why. Turning it on
/// asks first: Cancel, or closing the alert, puts the switch back; Turn Sync On
/// saves.
pub(super) fn the_sync_page_turns_sync_off_and_asks_to_turn_it_on(dir: &Path) {
    let rig = sync_rig(dir, true, Some(SYNC_KEY));
    let (ctx, page, window) = (&rig.ctx, &rig.page, &rig.window);

    // The first group of the page; on to begin with (a file that records
    // nothing means on).
    let groups = all_widgets::<adw::PreferencesGroup>(&page.widget);
    assert_eq!(groups[0].title(), "Sync", "first on the page");
    assert_eq!(page.sync_switch.title(), "Sync");
    assert!(!page.sync_switch.uses_markup());
    assert!(page.sync_switch.is_active());
    assert_eq!(
        page.sync_switch.subtitle().as_deref(),
        Some(sync::switch_subtitle(SyncSwitch::On))
    );
    assert!(page.sync_button.is_sensitive());
    assert!(page.auto_sync.is_sensitive());
    assert!(!page.hint.is_visible());

    // Off: saved at once, no question.
    page.sync_switch.set_active(false);
    pump_until("the file to say off", || {
        switch_on_disk(ctx) == SyncSwitch::Off
    });
    pump_until("the page to follow the file", || {
        !page.sync_button.is_sensitive()
    });
    assert!(
        all_widgets::<adw::AlertDialog>(window).is_empty(),
        "no alert"
    );
    assert_eq!(ctx.store.borrow().sync_switch(), SyncSwitch::Off);
    assert!(!page.sync_switch.is_active());
    assert_eq!(
        page.sync_switch.subtitle().as_deref(),
        Some(sync::switch_subtitle(SyncSwitch::Off))
    );
    assert!(!page.auto_sync.is_sensitive(), "Sync Automatically is off");
    assert!(page.hint.is_visible());
    assert_eq!(page.hint.label(), sync::SYNC_IS_OFF);
    assert!(
        page.machine.is_sensitive() && page.key_entry.is_sensitive(),
        "the key and the other machine can still be chosen"
    );

    // On: asks first. Nothing is written yet. The keyboard is on the row, as a
    // person who reached it with Tab and pressed Space has it.
    page.sync_switch.grab_focus();
    assert!(
        has_focus_in(window, &page.sync_switch),
        "the row has the keyboard before the question"
    );
    page.sync_switch.set_active(true);
    let alert = alert_in(window);
    assert_eq!(alert.heading().as_deref(), Some("Turn Sync On?"));
    assert!(!alert.is_heading_use_markup());
    assert_eq!(alert.body().as_str(), TURN_ON_BODY);
    assert!(
        alert
            .body()
            .contains("including any added while sync was off")
    );
    assert!(alert.body().contains("on its page first"));
    assert!(!alert.is_body_use_markup());
    assert_eq!(alert.default_response().as_deref(), Some("cancel"));
    assert_eq!(alert.close_response().as_str(), "cancel");
    assert_eq!(alert.response_label("cancel").as_str(), "Cancel");
    assert_eq!(alert.response_label("on").as_str(), "Turn Sync On");
    assert_eq!(
        alert.response_appearance("on"),
        adw::ResponseAppearance::Suggested
    );
    pump_for(Duration::from_millis(300));
    assert_eq!(switch_on_disk(ctx), SyncSwitch::Off);
    assert!(
        !has_focus_in(window, &page.sync_switch),
        "the question has the keyboard while it is open"
    );

    // Cancel: the switch goes back, nothing is written.
    button_labeled(&alert, "Cancel")
        .expect("the Cancel button")
        .emit_clicked();
    pump_until("the alert to go", || {
        all_widgets::<adw::AlertDialog>(window).is_empty()
    });
    assert!(!page.sync_switch.is_active(), "back to off");
    assert_eq!(switch_on_disk(ctx), SyncSwitch::Off);
    assert!(!page.sync_button.is_sensitive());
    // The row stays sensitive, so the keyboard comes back to it: Space again
    // asks again, with no mouse.
    pump_until("the keyboard back on the row after Cancel", || {
        has_focus_in(window, &page.sync_switch)
    });

    // Closing the alert (Escape) is Cancel.
    page.sync_switch.set_active(true);
    let alert = alert_in(window);
    alert.close();
    pump_until("the alert to go", || {
        all_widgets::<adw::AlertDialog>(window).is_empty()
    });
    assert!(!page.sync_switch.is_active(), "back to off");
    assert_eq!(switch_on_disk(ctx), SyncSwitch::Off);
    pump_until("the keyboard back on the row after Escape", || {
        has_focus_in(window, &page.sync_switch)
    });

    // Turn Sync On: saved, and everything comes back.
    page.sync_switch.set_active(true);
    let alert = alert_in(window);
    button_labeled(&alert, "Turn Sync On")
        .expect("the Turn Sync On button")
        .emit_clicked();
    pump_until("the file to say on", || {
        switch_on_disk(ctx) == SyncSwitch::On
    });
    pump_until("the page to follow the file", || {
        page.sync_button.is_sensitive()
    });
    assert!(page.sync_switch.is_active());
    assert_eq!(
        page.sync_switch.subtitle().as_deref(),
        Some(sync::switch_subtitle(SyncSwitch::On))
    );
    assert!(page.auto_sync.is_sensitive());
    assert!(!page.hint.is_visible());
    assert_eq!(ctx.store.borrow().sync_switch(), SyncSwitch::On);
    rig.finish();
}

/// A switch changed by `moniker-server sync-switch` or another Moniker (the
/// file watch tells the page) is followed by the page, by Sync Now and by the
/// page's controls.
pub(super) fn the_sync_page_follows_a_switch_changed_elsewhere(dir: &Path) {
    let (ctx, _secrets, profile) = sync_ctx(dir, true, Some(SYNC_KEY));
    set_partner(&ctx, &profile.expect("a profile"), true);
    let app = test_app();
    crate::add_actions(&app, &ctx);
    ctx.watch_profile_file();
    let window = open_sync_page(&app, &ctx);
    let page = page_in(&window);
    pump_until("the page to read the key's state", || {
        page.key_status.label() != sync::KEY_CHECKING
    });
    assert!(page.sync_switch.is_active());
    assert!(sync_now_enabled(&app));
    let path = ctx.store.borrow().path().to_path_buf();

    // Another handle writes the file; nobody calls `profiles_changed`.
    ProfileStore::open(&path).unwrap().set_sync(false).unwrap();
    pump_until("the page to follow the file", || {
        !page.sync_switch.is_active() && !sync_now_enabled(&app)
    });
    assert!(sync_now_exists(&app), "Sync Now stays, grayed out");
    assert_eq!(
        page.sync_switch.subtitle().as_deref(),
        Some(sync::switch_subtitle(SyncSwitch::Off))
    );
    assert_eq!(page.hint.label(), sync::SYNC_IS_OFF);
    assert!(!page.sync_button.is_sensitive());
    assert!(!page.auto_sync.is_sensitive());

    // And back on (no question: someone else asked it).
    ProfileStore::open(&path).unwrap().set_sync(true).unwrap();
    pump_until("the page to follow the file", || {
        page.sync_switch.is_active() && sync_now_enabled(&app)
    });
    assert!(all_widgets::<adw::AlertDialog>(&window).is_empty());
    assert!(page.sync_button.is_sensitive());
    assert!(page.auto_sync.is_sensitive());
    assert!(!page.hint.is_visible());
    drop(page);
    close_all(&app);
}

/// A switch that is recorded but cannot be read counts as off and says so;
/// turning sync on replaces it (and only then is the file touched).
pub(super) fn a_damaged_switch_shows_off_and_says_so(dir: &Path) {
    const DAMAGED: &str = r#"{"version":2,"profiles":[]}"#;
    fs::create_dir_all(dir).expect("dir");
    fs::write(dir.join("profiles.json"), DAMAGED).expect("write the file");
    let rig = sync_rig(dir, false, Some(SYNC_KEY));
    let (ctx, page, window) = (&rig.ctx, &rig.page, &rig.window);
    assert_eq!(ctx.store.borrow().sync_switch(), SyncSwitch::Unreadable);
    assert!(!page.sync_switch.is_active());
    assert_eq!(
        page.sync_switch.subtitle().as_deref(),
        Some(sync::switch_subtitle(SyncSwitch::Unreadable))
    );
    assert_eq!(page.hint.label(), sync::SYNC_IS_OFF);
    assert!(!page.auto_sync.is_sensitive());
    assert!(!page.sync_button.is_sensitive());
    let file = dir.join("profiles.json");
    assert_eq!(fs::read_to_string(&file).unwrap(), DAMAGED, "untouched");

    page.sync_switch.set_active(true);
    let alert = alert_in(window);
    button_labeled(&alert, "Turn Sync On")
        .expect("the Turn Sync On button")
        .emit_clicked();
    pump_until("the file to say on", || {
        switch_on_disk(ctx) == SyncSwitch::On
    });
    pump_until("the page to follow the file", || {
        page.sync_switch.subtitle().as_deref() == Some(sync::switch_subtitle(SyncSwitch::On))
    });
    assert!(page.sync_switch.is_active());
    assert!(page.auto_sync.is_sensitive());
    // On, so the hint is about what else is missing: no profile to sync with.
    assert!(page.hint.is_visible());
    assert_ne!(page.hint.label(), sync::SYNC_IS_OFF);
    rig.finish();
}

/// A save that fails is told near the page, and the switch goes back to what
/// the file says, whichever way it was flipped.
pub(super) fn a_switch_that_cannot_be_saved_goes_back_and_says_so(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let rig = sync_rig(dir, true, Some(SYNC_KEY));
    let (ctx, page, window) = (&rig.ctx, &rig.page, &rig.window);
    // The file cannot be replaced (its folder is read-only): every save fails.
    fs::set_permissions(dir, fs::Permissions::from_mode(0o500)).expect("chmod");

    page.sync_switch.set_active(false);
    pump_until("the failure to be told", || ctx.notice.delivered() > 0);
    assert!(
        ctx.notice
            .last_text()
            .is_some_and(|text| text.starts_with("Could not save the sync setting")),
        "{:?}",
        ctx.notice.last_text()
    );
    pump_until("the switch to go back", || page.sync_switch.is_active());
    assert!(page.sync_button.is_sensitive(), "still on");
    assert!(page.auto_sync.is_sensitive());

    // The same turning it on (the file says off): after the question.
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).expect("chmod back");
    ProfileStore::open(ctx.store.borrow().path())
        .unwrap()
        .set_sync(false)
        .unwrap();
    ctx.profiles_changed();
    assert!(!page.sync_switch.is_active());
    fs::set_permissions(dir, fs::Permissions::from_mode(0o500)).expect("chmod");
    let told = ctx.notice.delivered();
    page.sync_switch.set_active(true);
    let alert = alert_in(window);
    button_labeled(&alert, "Turn Sync On")
        .expect("the Turn Sync On button")
        .emit_clicked();
    pump_until("the failure to be told", || ctx.notice.delivered() > told);
    pump_until("the switch to go back", || !page.sync_switch.is_active());
    assert!(!page.sync_button.is_sensitive(), "still off");
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).expect("chmod back");
    assert_eq!(switch_on_disk(ctx), SyncSwitch::Off);
    rig.finish();
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
    super::launch_at_home(&app, &ctx);
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
    super::launch_at_home(&app, &ctx);
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
    let profiles_window = profiles::present(&app, &ctx, Rc::new(|_, _| {}));
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
    // Opening the page shows whether a key is set: that reads the keyring
    // (through the context's store, on the keyring worker). Count from once
    // it has.
    let profiles_window = open_sync_page(&app, &ctx);
    let page = page_in(&profiles_window);
    pump_until("the page to read the key's state", || {
        page.key_status.label() != sync::KEY_CHECKING
    });
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
    check("the sync on launch", &|| super::launch_at_home(&app, &ctx));
    drop(page);
    close_all(&app);
}

/// The sync page calls the keyring on the keyring worker, never on the main
/// thread (issue #4): opening it reads whether a sync key is set, and saving a
/// key stores it. Each shows its outcome once the worker is back.
pub(super) fn the_sync_page_uses_the_keyring_off_the_main_thread(dir: &Path) {
    let before = keyring::main_thread_calls();
    let rig = sync_rig(dir, true, Some(SYNC_KEY));
    pump_until("the key's state", || {
        rig.page.key_status.label() == "Sync key: set"
    });
    assert_eq!(
        keyring::main_thread_calls(),
        before,
        "opening the sync page called the keyring on the main thread"
    );
    rig.page.key_entry.set_text("a brand new sync key");
    rig.page.key_entry.emit_by_name::<()>("apply", &[]);
    pump_until("the key to be saved", || {
        rig.page.key_message.label() == "Sync key saved."
    });
    assert_eq!(
        rig.secrets
            .get(SYNC_PASSPHRASE_KEY)
            .unwrap()
            .expect("stored")
            .as_str(),
        "a brand new sync key"
    );
    assert_eq!(rig.page.key_entry.text(), "", "the field is cleared");
    assert_eq!(
        keyring::main_thread_calls(),
        before,
        "saving the sync key called the keyring on the main thread"
    );
    rig.finish();
}

/// While the sync page waits for the keyring it promises nothing: until the
/// first answer the status says the keyring is being read, no hint says to set
/// a key, and Sync is off; while a new key is being saved, Sync is off too, so
/// that no sync starts with the key being replaced.
pub(super) fn the_sync_page_waits_for_the_keyring(dir: &Path) {
    fs::create_dir_all(dir).expect("dir");
    let mut store = ProfileStore::open(&dir.join("profiles.json")).expect("open the store");
    let profile = Profile::new(NAME, "app.example.test", "alice");
    store.upsert(profile).expect("save the profile");
    let seeded = Arc::new(MemorySecretStore::default());
    seeded
        .set(SYNC_PASSPHRASE_KEY, SYNC_KEY)
        .expect("set the key");
    let keyring = Arc::new(HeldKeyring::new(seeded));
    let ctx = Rc::new(AppContext::with_parts(
        store,
        Arc::clone(&keyring) as Arc<dyn SecretStore>,
        make_helper(dir),
    ));
    let app = test_app();

    keyring.hold();
    let window = open_sync_page(&app, &ctx);
    let page = page_in(&window);
    assert_eq!(page.key_status.label(), sync::KEY_CHECKING);
    assert!(
        !page.hint.is_visible(),
        "no hint before the keyring answers"
    );
    assert!(!page.sync_button.is_sensitive());
    keyring.release();
    pump_until("the key's state", || {
        page.key_status.label() == "Sync key: set"
    });
    assert!(page.sync_button.is_sensitive());

    keyring.hold();
    page.key_entry.set_text("a replacement sync key");
    page.key_entry.emit_by_name::<()>("apply", &[]);
    assert!(
        !page.sync_button.is_sensitive(),
        "no sync while the key is being saved"
    );
    keyring.release();
    pump_until("the key to be saved", || {
        page.key_message.label() == "Sync key saved."
    });
    pump_until("Sync to be on again", || page.sync_button.is_sensitive());
    window.destroy();
    pump_for(Duration::from_millis(100));
}

/// The switch is in the profile file, not the keyring: with the keyring held
/// (its unlock prompt open), a page whose switch is off already says so, and a
/// switch the person turns on shows what they asked for, even if something
/// re-reads the file, until the save (queued behind the keyring on the one
/// worker) is back.
pub(super) fn the_sync_switch_does_not_wait_for_the_keyring(dir: &Path) {
    fs::create_dir_all(dir).expect("dir");
    let mut store = ProfileStore::open(&dir.join("profiles.json")).expect("open the store");
    store
        .upsert(Profile::new(NAME, "app.example.test", "alice"))
        .expect("save the profile");
    store.set_sync(false).expect("turn sync off");
    let seeded = Arc::new(MemorySecretStore::default());
    seeded
        .set(SYNC_PASSPHRASE_KEY, SYNC_KEY)
        .expect("set the key");
    let keyring = Arc::new(HeldKeyring::new(seeded));
    let ctx = Rc::new(AppContext::with_parts(
        store,
        Arc::clone(&keyring) as Arc<dyn SecretStore>,
        make_helper(dir),
    ));
    let app = test_app();

    keyring.hold();
    let window = open_sync_page(&app, &ctx);
    let page = page_in(&window);
    assert_eq!(page.key_status.label(), sync::KEY_CHECKING);
    assert!(page.hint.is_visible(), "the hint does not wait");
    assert_eq!(page.hint.label(), sync::SYNC_IS_OFF);
    assert!(!page.sync_switch.is_active());
    assert!(!page.auto_sync.is_sensitive());
    assert!(!page.sync_button.is_sensitive());

    page.sync_switch.set_active(true);
    let alert = alert_in(&window);
    // Something re-reads the file while the question is up: the row snaps to
    // what the file says, and Turn Sync On shows the answer again.
    ctx.profiles_changed();
    assert!(!page.sync_switch.is_active());
    button_labeled(&alert, "Turn Sync On")
        .expect("the Turn Sync On button")
        .emit_clicked();
    pump_until("the alert to go", || {
        all_widgets::<adw::AlertDialog>(&window).is_empty()
    });
    // The save waits behind the keyring; the file still says off.
    assert!(page.sync_switch.is_active(), "shows the answer at once");
    ctx.profiles_changed();
    assert_eq!(switch_on_disk(&ctx), SyncSwitch::Off);
    assert!(
        page.sync_switch.is_active(),
        "what was asked for, meanwhile"
    );
    keyring.release();
    pump_until("the file to say on", || {
        switch_on_disk(&ctx) == SyncSwitch::On
    });
    pump_until("the page to be ready", || {
        page.key_status.label() == "Sync key: set" && page.sync_button.is_sensitive()
    });
    assert!(page.sync_switch.is_active());
    assert!(!page.hint.is_visible());
    drop(page);
    window.destroy();
    pump_for(Duration::from_millis(100));
}

/// Issue #10: the main thread's snapshot of the profile file follows it only
/// when the file watch reloads it, a moment after another process (the command
/// line) turned sync off. A sync that starts in that moment is stopped by its
/// worker, which looks at the file before it asks the keyring for anything:
/// neither the sync key nor the partner's password is read, no ssh runs, and
/// the person is told the same sentence as for any refusal.
pub(super) fn a_sync_started_on_a_stale_snapshot_stops_before_the_keyring(dir: &Path) {
    let reads = Reads::default();
    let (ctx, secrets, profile) = sync_ctx_counting(dir, true, Some(SYNC_KEY), &reads);
    let profile = profile.expect("a profile");
    secrets
        .set(&password_key(&profile.id), "pw-for-ssh")
        .unwrap();
    set_partner(&ctx, &profile, true);
    let app = test_app();
    crate::add_actions(&app, &ctx);
    let area = super::bare_area();
    ctx.notice.attach(&area);
    let window = open_sync_page(&app, &ctx);
    let page = page_in(&window);
    pump_until("the page to read the key's state", || {
        page.key_status.label() != sync::KEY_CHECKING
    });
    assert!(page.sync_button.is_sensitive());

    // Another process turns sync off. Nobody reloads the snapshot (the file
    // watch would, after its delay): it still says on.
    ProfileStore::open(ctx.store.borrow().path())
        .unwrap()
        .set_sync(false)
        .unwrap();
    assert_eq!(switch_on_disk(&ctx), SyncSwitch::Off);
    assert!(ctx.store.borrow().sync_switch().is_on(), "a stale snapshot");
    assert!(page.sync_button.is_sensitive());
    assert!(sync_now_enabled(&app));

    // The Sync button: the sync starts, and its worker stops it.
    let runs = Arc::new(AtomicUsize::new(0));
    let worker_reads = reads.worker.load(Ordering::SeqCst);
    set_runner(&ctx, counting(&runs, || Ok(added(1))));
    page.sync_button.emit_clicked();
    assert!(
        ctx.sync_running.get(),
        "the snapshot said on, so it started"
    );
    pump_until("the sync to stop", || !ctx.sync_running.get());
    assert_eq!(page.result.label(), sync::SYNC_IS_OFF);
    assert!(has_css(&page.result, "error"));
    assert_eq!(runs.load(Ordering::SeqCst), 0, "no ssh");
    assert_eq!(
        reads.worker.load(Ordering::SeqCst),
        worker_reads,
        "neither the sync key nor the partner's password was read"
    );

    // Sync Now.
    set_runner(&ctx, counting(&runs, || Ok(added(1))));
    app.activate_action("sync-now", None);
    pump_until("Sync Now to say why", || {
        ctx.notice.last_text().as_deref()
            == Some("Sync failed: Sync is turned off on this machine.")
    });
    pump_until("it to stop", || !ctx.sync_running.get());
    assert_eq!(runs.load(Ordering::SeqCst), 0, "no ssh");
    assert_eq!(
        reads.worker.load(Ordering::SeqCst),
        worker_reads,
        "nothing read for Sync Now either"
    );
    drop(page);
    close_all(&app);
}

/// Issue #10: while the save of a switch turned off is still out (it runs on
/// the keyring worker, queued here behind the profile file's lock, which a sync
/// holds while it waits at the keyring's unlock prompt), the snapshot still says
/// on. What the person asked for counts at once: Sync, Sync Automatically and
/// Sync Now go off with the hint, and no sync starts, whichever way it is asked
/// for.
pub(super) fn an_off_request_counts_before_its_save_is_back(dir: &Path) {
    let reads = Reads::default();
    let (ctx, _secrets, profile) = sync_ctx_counting(dir, true, Some(SYNC_KEY), &reads);
    set_partner(&ctx, &profile.expect("a profile"), true);
    let app = test_app();
    crate::add_actions(&app, &ctx);
    let area = super::bare_area();
    ctx.notice.attach(&area);
    let window = open_sync_page(&app, &ctx);
    let page = page_in(&window);
    pump_until("the page to read the key's state", || {
        page.key_status.label() != sync::KEY_CHECKING
    });
    assert!(page.sync_button.is_sensitive());
    assert!(page.auto_sync.is_sensitive());
    assert!(sync_now_enabled(&app));
    let (runs, delivered) = (Arc::new(AtomicUsize::new(0)), ctx.notice.delivered());

    let held = HeldProfileFile::hold(dir);
    page.sync_switch.set_active(false);
    // The save is waiting for the file: it and the snapshot say on still.
    assert_eq!(switch_on_disk(&ctx), SyncSwitch::On);
    assert!(ctx.store.borrow().sync_switch().is_on());
    assert!(!page.sync_switch.is_active());
    assert!(!page.sync_button.is_sensitive(), "Sync is off at once");
    assert!(
        !page.auto_sync.is_sensitive(),
        "and so is Sync Automatically"
    );
    assert!(page.hint.is_visible());
    assert_eq!(page.hint.label(), sync::SYNC_IS_OFF);
    assert!(sync_now_exists(&app));
    assert!(!sync_now_enabled(&app), "Sync Now is grayed out at once");

    // Whichever way a sync is asked for, nothing starts and nothing is read.
    let worker_reads = reads.worker.load(Ordering::SeqCst);
    set_runner(&ctx, counting(&runs, || Ok(added(1))));
    page.sync_button.emit_clicked();
    assert_eq!(page.result.label(), sync::SYNC_IS_OFF);
    assert!(!ctx.sync_running.get());
    crate::sync_now(&app, &ctx);
    assert!(!ctx.sync_running.get());
    assert_eq!(
        ctx.notice.last_text().as_deref(),
        Some("Sync failed: Sync is turned off on this machine.")
    );
    let told = ctx.notice.delivered();
    assert_eq!(told, delivered + 1);
    crate::sync_on_launch(&ctx, app.hold());
    pump_for(Duration::from_millis(300));
    assert!(!ctx.sync_running.get(), "no sync on launch");
    assert_eq!(ctx.notice.delivered(), told, "and nothing said about it");
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert_eq!(reads.worker.load(Ordering::SeqCst), worker_reads);
    assert_eq!(switch_on_disk(&ctx), SyncSwitch::On, "still waiting");

    // The save lands: the page and the menu agree with the file.
    held.release();
    pump_until("the file to say off", || {
        switch_on_disk(&ctx) == SyncSwitch::Off
    });
    pump_until("the page to follow the file", || {
        ctx.store.borrow().sync_switch() == SyncSwitch::Off
    });
    assert!(!page.sync_switch.is_active());
    assert!(!page.sync_button.is_sensitive());
    assert!(!sync_now_enabled(&app));
    assert_eq!(page.hint.label(), sync::SYNC_IS_OFF);
    drop(page);
    close_all(&app);
}

/// Issue #10: Cancel on "Turn Sync On?" (or Escape) while an earlier off-save
/// is still out leaves the row showing the last thing asked for, off, and not
/// what the file still says, on. Turn Sync On in the same state is the last
/// thing asked for, and ends on.
pub(super) fn cancelling_the_question_leaves_the_row_at_the_last_request(dir: &Path) {
    let rig = sync_rig(dir, true, Some(SYNC_KEY));
    let (ctx, page, window) = (&rig.ctx, &rig.page, &rig.window);
    let held = HeldProfileFile::hold(dir);

    page.sync_switch.set_active(false);
    assert!(!page.sync_switch.is_active());
    assert_eq!(switch_on_disk(ctx), SyncSwitch::On, "the save is out");

    // Flip it on again: the question. Cancel puts the row back to off.
    page.sync_switch.set_active(true);
    let alert = alert_in(window);
    button_labeled(&alert, "Cancel")
        .expect("the Cancel button")
        .emit_clicked();
    pump_until("the alert to go", || {
        all_widgets::<adw::AlertDialog>(window).is_empty()
    });
    assert!(
        !page.sync_switch.is_active(),
        "off, what was last asked for: not on, which the file still says"
    );
    assert!(!page.sync_button.is_sensitive());

    // Escape is Cancel.
    page.sync_switch.set_active(true);
    alert_in(window).close();
    pump_until("the alert to go", || {
        all_widgets::<adw::AlertDialog>(window).is_empty()
    });
    assert!(!page.sync_switch.is_active(), "off after Escape too");
    assert_eq!(switch_on_disk(ctx), SyncSwitch::On, "nothing written yet");

    // Turn Sync On: the last request is on, and both saves land in order.
    page.sync_switch.set_active(true);
    let alert = alert_in(window);
    button_labeled(&alert, "Turn Sync On")
        .expect("the Turn Sync On button")
        .emit_clicked();
    pump_until("the alert to go", || {
        all_widgets::<adw::AlertDialog>(window).is_empty()
    });
    assert!(page.sync_switch.is_active());
    held.release();
    pump_until("the saves to land", || keyring::pending() == 0);
    pump_until("the page to follow the file", || {
        page.sync_button.is_sensitive()
    });
    assert_eq!(switch_on_disk(ctx), SyncSwitch::On);
    assert!(page.sync_switch.is_active());
    assert!(page.auto_sync.is_sensitive());
    assert!(!page.hint.is_visible());
    rig.finish();
}

/// What a runner of these scenarios saw: whether each sync it ran accepted a
/// machine that has been away.
type Choices = Arc<Mutex<Vec<StaleSync>>>;

/// A runner that says this machine has been away unless the sync accepts it.
fn away_unless_allowed(ctx: &AppContext, seen: &Choices) {
    let seen = Arc::clone(seen);
    set_runner(ctx, move |inputs: SyncInputs| {
        seen.lock().unwrap().push(inputs.stale);
        match inputs.stale {
            StaleSync::Refuse => Err(SyncError::Stale { days: 200 }),
            StaleSync::Allow => Ok(added(1)),
        }
    });
}

const AWAY: &str = "This machine has not synced for 200 days. Moniker forgets a deletion a year \
    after it is made, so a profile deleted on another machine more than a year ago could come \
    back on every machine.";

/// Sync Now on a machine that has been away asks first (issue #8): Sync Anyway
/// syncs again accepting it, Cancel does nothing more.
pub(super) fn sync_now_asks_a_machine_that_has_been_away(dir: &Path) {
    let (ctx, _secrets, profile) = sync_ctx(dir, true, Some(SYNC_KEY));
    set_partner(&ctx, &profile.expect("a profile"), true);
    let app = test_app();
    crate::add_actions(&app, &ctx);
    let main = window::build(&app, &ctx);
    main.window.present();
    pump_for(Duration::from_millis(200));

    let seen: Choices = Arc::default();
    away_unless_allowed(&ctx, &seen);
    app.activate_action("sync-now", None);
    let alert = alert_in(&main.window);
    assert_eq!(alert.heading().as_deref(), Some("Sync Anyway?"));
    assert!(!alert.is_heading_use_markup());
    assert_eq!(alert.body().as_str(), AWAY);
    assert!(!alert.is_body_use_markup());
    away_unless_allowed(&ctx, &seen);
    button_labeled(&alert, "Sync Anyway")
        .expect("the Sync Anyway button")
        .emit_clicked();
    pump_until("the second sync", || seen.lock().unwrap().len() == 2);
    pump_until("it to finish", || !ctx.sync_running.get());
    pump_for(Duration::from_millis(100));
    assert_eq!(*seen.lock().unwrap(), [StaleSync::Refuse, StaleSync::Allow]);
    assert_eq!(
        ctx.notice.last_text().as_deref(),
        Some("Synced: 1 added, 0 updated, 0 removed.")
    );
    pump_until("the alert to go", || {
        all_widgets::<adw::AlertDialog>(&main.window).is_empty()
    });

    // Cancel: nothing more runs.
    seen.lock().unwrap().clear();
    away_unless_allowed(&ctx, &seen);
    app.activate_action("sync-now", None);
    let alert = alert_in(&main.window);
    button_labeled(&alert, "Cancel")
        .expect("the Cancel button")
        .emit_clicked();
    pump_until("the alert to close", || {
        main.window.visible_dialog().is_none()
    });
    pump_for(Duration::from_millis(300));
    assert_eq!(*seen.lock().unwrap(), [StaleSync::Refuse]);
    main.window.destroy();
    pump_for(Duration::from_millis(100));
}

/// The sync page asks the same, over the Connection Profiles window, and its
/// line says why.
pub(super) fn the_sync_page_asks_a_machine_that_has_been_away(dir: &Path) {
    let (ctx, _secrets, _profile) = sync_ctx(dir, true, Some(SYNC_KEY));
    let app = test_app();
    let profiles_window = open_sync_page(&app, &ctx);
    let page = page_in(&profiles_window);
    pump_until("the key status", || page.sync_button.is_sensitive());

    let seen: Choices = Arc::default();
    away_unless_allowed(&ctx, &seen);
    page.sync_button.emit_clicked();
    let alert = alert_in(&profiles_window);
    assert_eq!(alert.body().as_str(), AWAY);
    assert_eq!(page.result.label(), AWAY);
    away_unless_allowed(&ctx, &seen);
    button_labeled(&alert, "Sync Anyway")
        .expect("the Sync Anyway button")
        .emit_clicked();
    pump_until("the second sync", || seen.lock().unwrap().len() == 2);
    pump_until("it to finish", || !ctx.sync_running.get());
    pump_for(Duration::from_millis(100));
    assert_eq!(*seen.lock().unwrap(), [StaleSync::Refuse, StaleSync::Allow]);
    assert_eq!(
        page.result.label(),
        "Synced: 1 added, 0 updated, 0 removed."
    );
    drop(page);
    profiles_window.destroy();
    pump_for(Duration::from_millis(100));
}

/// The sync at launch never asks: one notification says why it did not sync
/// and how to sync anyway.
pub(super) fn the_launch_sync_of_a_machine_that_has_been_away_only_says_so(dir: &Path) {
    let (ctx, _secrets, profile) = sync_ctx(dir, true, Some(SYNC_KEY));
    set_partner(&ctx, &profile.expect("a profile"), true);
    let app = test_app();
    let seen: Choices = Arc::default();
    away_unless_allowed(&ctx, &seen);
    let delivered = ctx.notice.delivered();
    super::launch_at_home(&app, &ctx);
    pump_until("the notification", || ctx.notice.delivered() > delivered);
    pump_for(Duration::from_millis(300));
    assert_eq!(*seen.lock().unwrap(), [StaleSync::Refuse]);
    assert_eq!(ctx.notice.delivered(), delivered + 1);
    assert_eq!(
        ctx.notice.last_text().as_deref(),
        Some(format!("Did not sync automatically. {AWAY} Use Sync Now to sync anyway.").as_str())
    );
    assert!(
        app.windows().iter().all(|w| w
            .downcast_ref::<adw::ApplicationWindow>()
            .is_none_or(|w| w.visible_dialog().is_none())),
        "no alert"
    );
    close_all(&app);
}
