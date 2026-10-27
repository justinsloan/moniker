//! Scenarios: the Connection Profiles window (sidebar, profile page with per-field
//! saving, New Profile dialog, delete).
//!
//! Each opens [`profiles::present`] over a temporary store, an in-memory
//! keyring that counts reads, and a [`ConnectFn`] that only records what it
//! was asked to connect.

use std::cell::{Cell, RefCell};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;
use moniker_core::profile::{Profile, StartupSequence};
use moniker_core::secrets::{MemorySecretStore, SecretError, SecretStore, password_key};
use moniker_core::store::ProfileStore;
use zeroize::Zeroizing;

use super::windows::list_in;
use super::{
    HeldKeyring, HeldProfileFile, MARKUP_NAME, alert_in, all_widgets, button_labeled, find_widget,
    has_focus_in, literal_labels, make_helper, pump_for, pump_until, row_for,
};
use crate::context::AppContext;
use crate::local_terminal::{self, LOCAL_SECRETS_WARNING, LOCAL_SEQUENCES_NOTE, LocalTerminal};
use crate::profiles::{self, ConnectFn, Pick};
use crate::{form, keyring, new_tab, settings, window};

/// An in-memory keyring that counts `get` calls: the profile page must never
/// read the keyring.
#[derive(Default)]
struct CountingSecrets {
    inner: MemorySecretStore,
    gets: AtomicUsize,
}

impl CountingSecrets {
    fn gets(&self) -> usize {
        self.gets.load(Ordering::SeqCst)
    }

    /// The stored password of `id`, read without counting.
    fn password(&self, id: &str) -> Option<String> {
        self.inner
            .get(&password_key(id))
            .expect("memory store")
            .map(|p| p.as_str().to_owned())
    }
}

impl SecretStore for CountingSecrets {
    fn get(&self, key: &str) -> Result<Option<Zeroizing<String>>, SecretError> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        self.inner.get(key)
    }
    fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        self.inner.set(key, value)
    }
    fn delete(&self, key: &str) -> Result<(), SecretError> {
        self.inner.delete(key)
    }
    fn probe(&self) -> Result<(), SecretError> {
        Ok(())
    }
}

/// What the window asked to open, in the order asked.
#[derive(Clone, Debug, PartialEq)]
enum Opened {
    /// A profile by id, with the sequence (an index) and whether a transcript
    /// is saved.
    Profile(String, Option<usize>, bool),
    /// Local terminal, with the sequence by name and whether a transcript is
    /// saved.
    Local(Option<String>, bool),
}

type Connected = Rc<RefCell<Vec<Opened>>>;

/// An open Connection Profiles window and what is behind it.
struct Setup {
    dir: PathBuf,
    ctx: Rc<AppContext>,
    secrets: Arc<CountingSecrets>,
    window: adw::Window,
    connected: Connected,
    _app: adw::Application,
}

impl Setup {
    /// Open the window over a store holding `profiles`.
    fn open(dir: &Path, profiles: &[Profile]) -> Setup {
        Setup::open_with(dir, profiles, |secrets| secrets)
    }

    /// [`Setup::open`], with the context's keyring made by `keyring` around the
    /// counting one (which the test keeps reading without counting).
    fn open_with(
        dir: &Path,
        profiles: &[Profile],
        keyring: impl FnOnce(Arc<dyn SecretStore>) -> Arc<dyn SecretStore>,
    ) -> Setup {
        fs::create_dir_all(dir).expect("dir");
        let mut store = ProfileStore::open(&dir.join("profiles.json")).expect("store");
        for p in profiles {
            store.upsert(p.clone()).expect("save");
        }
        let secrets = Arc::new(CountingSecrets::default());
        let ctx = Rc::new(AppContext::with_parts(
            store,
            keyring(Arc::clone(&secrets) as Arc<dyn SecretStore>),
            make_helper(dir),
        ));
        let app = adw::Application::builder()
            .application_id("org.moniker.GuiTest.Profiles")
            .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(gtk::gio::Cancellable::NONE)
            .expect("register the test application");
        let connected: Connected = Rc::default();
        let record: ConnectFn = {
            let connected = Rc::clone(&connected);
            Rc::new(move |pick: Pick<'_>, transcript| {
                let opened = match pick {
                    Pick::Profile { profile, sequence } => {
                        Opened::Profile(profile.id.clone(), sequence, transcript)
                    }
                    Pick::Local { sequence } => Opened::Local(sequence.map(|s| s.name), transcript),
                };
                connected.borrow_mut().push(opened);
            })
        };
        let window = profiles::present(&app, &ctx, record);
        pump_for(Duration::from_millis(300));
        Setup {
            dir: dir.to_owned(),
            ctx,
            secrets,
            window,
            connected,
            _app: app,
        }
    }

    fn select(&self, id: &str) {
        profiles::select(&self.window, id);
        pump_for(Duration::from_millis(100));
    }

    fn sidebar(&self) -> gtk::ListBox {
        all_widgets::<gtk::ListBox>(&self.window)
            .into_iter()
            .find(|list| list.widget_name() == profiles::SIDEBAR_NAME)
            .expect("the sidebar list")
    }

    /// The sidebar rows, in order.
    fn rows(&self) -> Vec<adw::ActionRow> {
        let list = self.sidebar();
        let mut rows = Vec::new();
        let mut index = 0;
        while let Some(row) = list.row_at_index(index) {
            rows.push(row.downcast().expect("an action row"));
            index += 1;
        }
        rows
    }

    fn row_titles(&self) -> Vec<String> {
        self.rows().iter().map(|r| r.title().to_string()).collect()
    }

    /// The entry row titled `title` on the shown page.
    fn entry(&self, title: &str) -> adw::EntryRow {
        all_widgets::<adw::EntryRow>(&self.window)
            .into_iter()
            .find(|row| row.title() == title)
            .unwrap_or_else(|| panic!("no {title:?} row"))
    }

    /// The switch row titled `title` on the shown page.
    fn switch(&self, title: &str) -> adw::SwitchRow {
        all_widgets::<adw::SwitchRow>(&self.window)
            .into_iter()
            .find(|row| row.title() == title)
            .unwrap_or_else(|| panic!("no {title:?} switch"))
    }

    /// The profile `id` as it is in the file now.
    fn on_disk(&self, id: &str) -> Option<Profile> {
        ProfileStore::open(&self.dir.join("profiles.json"))
            .expect("store")
            .get(id)
            .cloned()
    }

    fn file_bytes(&self) -> Vec<u8> {
        fs::read(self.dir.join("profiles.json")).expect("profile file")
    }

    /// Another handle on the file, as a sync in another process has.
    fn other_store(&self) -> ProfileStore {
        ProfileStore::open(&self.dir.join("profiles.json")).expect("store")
    }

    /// The content pane's navigation view: the profile page, and over it the
    /// startup-sequence sub-page while that is open.
    fn content_nav(&self) -> adw::NavigationView {
        all_widgets::<adw::NavigationView>(&self.window)
            .into_iter()
            .find(|view| view.widget_name() == profiles::NAV_NAME)
            .expect("the content pane's navigation view")
    }

    /// The startup-sequence sub-page, if it is the page being shown.
    fn sequence_page(&self) -> Option<adw::NavigationPage> {
        self.content_nav()
            .visible_page()
            .filter(|page| page.tag().as_deref() == Some(profiles::SEQUENCE_TAG))
    }

    /// Whether a sub-page is still in the widget tree (a popped page stays
    /// until the transition ends).
    fn sequence_page_in_tree(&self) -> bool {
        all_widgets::<adw::NavigationPage>(&self.window)
            .iter()
            .any(|page| page.tag().as_deref() == Some(profiles::SEQUENCE_TAG))
    }

    /// The rows of the "Startup sequences" group, in order.
    fn sequence_rows(&self) -> Vec<adw::ActionRow> {
        let group = all_widgets::<adw::PreferencesGroup>(&self.window)
            .into_iter()
            .find(|group| group.title() == "Startup sequences")
            .expect("the Startup sequences group");
        // The default row is an action row too, but not one of the sequences.
        all_widgets::<adw::ActionRow>(&group)
            .into_iter()
            .filter(|row| !row.is::<adw::ComboRow>())
            .collect()
    }

    fn sequence_row_titles(&self) -> Vec<String> {
        self.sequence_rows()
            .iter()
            .map(|row| row.title().to_string())
            .collect()
    }

    /// The "Default sequence" row of the shown page (not on show while the
    /// profile has no sequence).
    fn default_row(&self) -> adw::ComboRow {
        all_widgets::<adw::ComboRow>(&self.window)
            .into_iter()
            .find(|row| row.title() == "Default sequence")
            .expect("the Default sequence row")
    }

    /// Choose `name` in the "Default sequence" row, as a click on its list
    /// does, and wait for what that saves.
    fn pick_default(&self, name: &str) {
        let row = self.default_row();
        let at = items_of(&row)
            .iter()
            .position(|item| item == name)
            .unwrap_or_else(|| panic!("no {name:?} in {:?}", items_of(&row)));
        row.set_selected(u32::try_from(at).expect("a small position"));
        saved();
    }

    /// The default sequence of profile `id` as the file has it.
    fn stored_default(&self, id: &str) -> Option<usize> {
        self.on_disk(id).expect("the profile").startup_sequence
    }

    /// Every row of the "Startup sequences" group, in order, the default row
    /// and the Add row included.
    fn group_row_titles(&self) -> Vec<String> {
        let group = all_widgets::<adw::PreferencesGroup>(&self.window)
            .into_iter()
            .find(|group| group.title() == "Startup sequences")
            .expect("the Startup sequences group");
        all_widgets::<adw::ActionRow>(&group)
            .iter()
            .map(|row| row.title().to_string())
            .collect()
    }

    /// Write `change` to the file as another process (a sync) would.
    fn elsewhere(&self, id: &str, change: impl FnOnce(&mut Profile)) {
        let mut profile = self.on_disk(id).expect("the profile");
        change(&mut profile);
        self.other_store().upsert(profile).expect("save");
    }

    /// Activate the sequence row titled `title` (a click on it) and return the
    /// sub-page it opens, once that is shown.
    fn open_sequence(&self, title: &str) -> adw::NavigationPage {
        let row = self
            .sequence_rows()
            .into_iter()
            .find(|row| row.title() == title)
            .unwrap_or_else(|| panic!("no {title:?} row"));
        ActionRowExt::activate(&row);
        pump_until("the sub-page", || {
            self.sequence_page().is_some_and(|page| page.is_mapped())
        });
        self.sequence_page().expect("the sub-page")
    }

    /// Click `label` on the sub-page, and wait for what it saves (on the
    /// keyring worker).
    fn click(&self, page: &adw::NavigationPage, label: &str) {
        button_labeled(page, label)
            .unwrap_or_else(|| panic!("no {label:?} button"))
            .emit_clicked();
        saved();
        pump_for(Duration::from_millis(50));
    }

    /// Wait until the sub-page has gone, transition and all.
    fn wait_for_sub_page_to_close(&self) {
        pump_until("the sub-page to close", || !self.sequence_page_in_tree());
    }

    /// The sequences of profile `id` as they are in the file, as
    /// (name, commands).
    fn stored_sequences(&self, id: &str) -> Vec<(String, Vec<String>)> {
        self.on_disk(id)
            .expect("the profile")
            .sequences
            .into_iter()
            .map(|s| (s.name, s.commands))
            .collect()
    }

    /// The sidebar's Local terminal entry (issue #13): its list, and the one
    /// row in it.
    fn local_entry(&self) -> (gtk::ListBox, adw::ActionRow) {
        let list = all_widgets::<gtk::ListBox>(&self.window)
            .into_iter()
            .find(|list| list.widget_name() == profiles::LOCAL_ENTRY_NAME)
            .expect("the Local terminal entry");
        let row = list
            .row_at_index(0)
            .and_downcast::<adw::ActionRow>()
            .expect("its row");
        (list, row)
    }

    /// Select the Local terminal entry, as a click on it does, which shows
    /// its page.
    fn select_local(&self) {
        let (list, row) = self.local_entry();
        list.select_row(Some(&row));
        pump_for(Duration::from_millis(100));
    }

    /// The content pane's title.
    fn title(&self) -> String {
        all_widgets::<gtk::Label>(&self.window)
            .into_iter()
            .find(|label| label.widget_name() == profiles::TITLE_NAME)
            .expect("the page title")
            .label()
            .to_string()
    }

    /// The groups of the page shown in the content pane, in order.
    fn page_groups(&self) -> Vec<adw::PreferencesGroup> {
        let page = all_widgets::<adw::PreferencesPage>(&self.window)
            .into_iter()
            .find(|page| page.is_mapped())
            .expect("a page in the content pane");
        all_widgets::<adw::PreferencesGroup>(&page)
    }

    /// Local terminal as the settings file has it now.
    fn local(&self) -> LocalTerminal {
        settings::load(&self.ctx.settings_path).local_terminal
    }

    /// Write Local terminal's `sequences` and `default` into the settings file,
    /// as another window (or a hand edit) would: nobody is told.
    fn local_elsewhere(&self, sequences: &[StartupSequence], default: Option<usize>) {
        settings::update(&self.ctx.settings_path, |x| {
            x.local_terminal.sequences = sequences.to_vec();
            x.local_terminal.startup_sequence = default;
        })
        .expect("write the settings");
    }

    fn settings_bytes(&self) -> Vec<u8> {
        fs::read(&self.ctx.settings_path).expect("the settings file")
    }

    /// How many times the lists and windows are told of a change from now on
    /// ([`AppContext::profiles_changed`]), while the window is open.
    fn heard(&self) -> Rc<Cell<usize>> {
        let heard = Rc::new(Cell::new(0));
        let count = Rc::clone(&heard);
        self.ctx
            .on_profiles_changed(&self.window, move || count.set(count.get() + 1));
        heard
    }

    fn shows_empty_state(&self) -> bool {
        all_widgets::<adw::StatusPage>(&self.window)
            .iter()
            .any(|page| page.title() == "No profile selected" && page.is_mapped())
    }

    fn close(self) {
        self.window.close();
        pump_for(Duration::from_millis(100));
    }
}

/// Click ✓ (or press Enter) on an entry row, and wait for its save.
fn apply(row: &impl IsA<adw::EntryRow>) {
    press_apply(row);
    saved();
}

/// Click ✓ on an entry row, without waiting for its save (the keyring or the
/// file is held).
fn press_apply(row: &impl IsA<adw::EntryRow>) {
    row.emit_by_name::<()>("apply", &[]);
    pump_for(Duration::from_millis(50));
}

/// Wait until every save asked for is back: they run on the keyring worker
/// (issue #7), and the page shows the outcome once they are.
fn saved() {
    pump_until("the saves", || keyring::pending() == 0);
}

/// Whether the ✓ of `row` is showing (libadwaita keeps it in a revealer).
fn apply_button_shown(row: &impl IsA<gtk::Widget>) -> bool {
    all_widgets::<gtk::Button>(row)
        .iter()
        .any(|b| b.has_css_class("apply-button") && b.is_child_visible())
}

/// The Name row of the sub-page.
fn sub_name(page: &adw::NavigationPage) -> adw::EntryRow {
    all_widgets::<adw::EntryRow>(page)
        .into_iter()
        .find(|row| row.title() == "Name")
        .expect("the sub-page's Name row")
}

fn commands_view(page: &adw::NavigationPage) -> gtk::TextView {
    find_widget(page).expect("the commands editor")
}

fn commands_text(page: &adw::NavigationPage) -> String {
    let buffer = commands_view(page).buffer();
    buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), false)
        .to_string()
}

fn set_commands(page: &adw::NavigationPage, text: &str) {
    commands_view(page).buffer().set_text(text);
}

/// The message in the sub-page's inline error label, if one is showing.
fn inline_error(page: &adw::NavigationPage) -> Option<String> {
    all_widgets::<gtk::Label>(page)
        .into_iter()
        .find(|label| label.has_css_class("error") && label.is_visible())
        .map(|label| label.label().to_string())
}

/// The description of the sub-page's Order group.
fn order_description(page: &adw::NavigationPage) -> Option<String> {
    all_widgets::<adw::PreferencesGroup>(page)
        .into_iter()
        .find(|group| group.title() == "Order")
        .expect("the Order group")
        .description()
        .map(|text| text.to_string())
}

fn seq(name: &str, commands: &[&str]) -> StartupSequence {
    StartupSequence {
        name: name.to_owned(),
        commands: commands.iter().map(|c| (*c).to_owned()).collect(),
    }
}

/// `web()` with these startup sequences.
fn web_with(sequences: &[StartupSequence]) -> Profile {
    let mut p = web();
    p.sequences = sequences.to_vec();
    p
}

fn stored(name: &str, commands: &[&str]) -> (String, Vec<String>) {
    (
        name.to_owned(),
        commands.iter().map(|c| (*c).to_owned()).collect(),
    )
}

fn web() -> Profile {
    Profile::new("web", "web.example.test", "alice")
}

pub(super) fn sidebar_lists_live_profiles_sorted_and_filters(dir: &Path) {
    let gone = Profile::new("gone", "gone.example.test", "carol");
    let profiles = [
        web(),
        Profile::new("NAS box", "nas.lan", "admin"),
        Profile::new("alpha", "alpha.example.test", "bob"),
        gone.clone(),
    ];
    fs::create_dir_all(dir).expect("dir");
    let s = Setup::open(dir, &profiles);
    s.other_store().delete(&gone.id).expect("delete");
    s.ctx.profiles_changed();

    let expected: Vec<String> = s
        .ctx
        .store
        .borrow()
        .live()
        .iter()
        .map(|p| p.name.clone())
        .collect();
    assert_eq!(expected, ["alpha", "NAS box", "web"]);
    assert_eq!(s.row_titles(), expected);
    assert_eq!(s.rows()[1].subtitle().as_deref(), Some("admin@nas.lan:22"));

    let search: gtk::SearchEntry = find_widget(&s.window).expect("the search entry");
    search.set_text("nas");
    pump_until("the filter to apply", || {
        s.rows().iter().filter(|r| r.is_child_visible()).count() == 1
    });
    let visible: Vec<String> = s
        .rows()
        .iter()
        .filter(|r| r.is_child_visible())
        .map(|r| r.title().to_string())
        .collect();
    assert_eq!(visible, ["NAS box"]);
    search.set_text("");
    pump_until("the filter to clear", || {
        s.rows().iter().all(|r| r.is_child_visible())
    });
    s.close();
}

pub(super) fn selecting_a_profile_reads_no_secret(dir: &Path) {
    let a = web();
    let b = Profile::new("nas", "nas.lan", "admin");
    let s = Setup::open(dir, &[a.clone(), b.clone()]);
    s.secrets.set(&password_key(&a.id), "pw-a").expect("set");
    s.secrets.set(&password_key(&b.id), "pw-b").expect("set");

    s.select(&a.id);
    assert_eq!(s.entry("Host").text(), a.host);
    s.select(&b.id);
    assert_eq!(s.entry("Host").text(), b.host);
    s.select(&a.id);

    assert_eq!(s.secrets.gets(), 0, "the profile page read the keyring");
    assert_eq!(s.entry("Password").text(), "");
    s.close();
}

pub(super) fn entry_apply_saves_only_that_field(dir: &Path) {
    let p = web();
    let s = Setup::open(dir, std::slice::from_ref(&p));
    s.select(&p.id);
    let host = s.entry("Host");
    let before = s.file_bytes();

    // A bad value: marked, explained, nothing written, text kept.
    host.grab_focus();
    host.set_text("bad host!");
    assert!(apply_button_shown(&host), "typing shows the apply button");
    apply(&host);
    assert!(host.has_css_class("error"));
    assert_eq!(
        s.ctx.notice.last_text().as_deref(),
        Some(
            "Host must not be empty, may use only letters, digits and . _ : % -, \
             and must not start with '-'."
        )
    );
    assert_eq!(s.file_bytes(), before);
    assert_eq!(host.text(), "bad host!");
    assert!(host.is_editable(), "editable again after a failed save");

    host.set_text("  new.example.test ");
    apply(&host);
    let stored = s.on_disk(&p.id).expect("still there");
    assert_eq!(stored.host, "new.example.test");
    assert_eq!(stored.name, p.name);
    assert_eq!(stored.username, p.username);
    assert!(!host.has_css_class("error"));
    assert_eq!(
        host.text(),
        "new.example.test",
        "the row shows what was stored"
    );
    assert!(!apply_button_shown(&host), "nothing left to apply");
    assert_eq!(s.entry("Name").text(), p.name);
    assert_eq!(s.entry("Username").text(), p.username);
    // The sidebar follows at once.
    assert_eq!(
        s.rows()[0].subtitle().as_deref(),
        Some("alice@new.example.test:22")
    );
    s.close();
}

pub(super) fn leaving_a_row_discards_the_edit(dir: &Path) {
    let p = web();
    let s = Setup::open(dir, std::slice::from_ref(&p));
    s.select(&p.id);
    let before = s.file_bytes();
    let name = s.entry("Name");

    name.grab_focus();
    name.set_text("typed but not applied");
    pump_for(Duration::from_millis(50));
    s.entry("Host").grab_focus();
    pump_for(Duration::from_millis(100));

    assert_eq!(name.text(), p.name);
    assert!(!apply_button_shown(&name));
    assert_eq!(s.file_bytes(), before);

    // Selecting another profile drops an edit too.
    let other = Profile::new("other", "other.lan", "bob");
    s.other_store().upsert(other.clone()).expect("save");
    s.ctx.profiles_changed();
    s.entry("Name").set_text("also not applied");
    s.select(&other.id);
    s.select(&p.id);
    assert_eq!(s.entry("Name").text(), p.name);
    assert_eq!(s.on_disk(&p.id).expect("there").name, p.name);
    s.close();
}

pub(super) fn password_row_clears_after_apply_and_on_leave(dir: &Path) {
    let p = web();
    let s = Setup::open(dir, std::slice::from_ref(&p));
    s.select(&p.id);
    let password = s.entry("Password");
    let stamp = s.on_disk(&p.id).expect("there").updated_at;

    password.grab_focus();
    password.set_text("s3cret-one");
    apply(&password);
    assert_eq!(s.secrets.password(&p.id).as_deref(), Some("s3cret-one"));
    assert_eq!(password.text(), "");
    assert!(
        s.on_disk(&p.id).expect("there").updated_at > stamp,
        "the profile is saved again so sync carries the password"
    );
    assert!(
        s.ctx
            .notice
            .last_text()
            .is_none_or(|t| !t.contains("s3cret")),
        "no message holds the password"
    );

    password.grab_focus();
    password.set_text("s3cret-two");
    s.entry("Host").grab_focus();
    pump_for(Duration::from_millis(100));
    assert_eq!(password.text(), "");
    assert_eq!(s.secrets.password(&p.id).as_deref(), Some("s3cret-one"));

    // Empty means keep: applying it changes nothing.
    let bytes = s.file_bytes();
    apply(&password);
    assert_eq!(s.file_bytes(), bytes);
    assert_eq!(s.secrets.password(&p.id).as_deref(), Some("s3cret-one"));

    // Closing the window leaves no typed password behind.
    password.set_text("s3cret-three");
    s.window.close();
    pump_for(Duration::from_millis(100));
    assert_eq!(password.text(), "");
    assert_eq!(s.secrets.password(&p.id).as_deref(), Some("s3cret-one"));
}

pub(super) fn switch_port_and_key_file_save_when_changed(dir: &Path) {
    let mut p = web();
    p.key_file = Some(PathBuf::from("/home/alice/.ssh/id_ed25519"));
    let s = Setup::open(dir, std::slice::from_ref(&p));
    s.select(&p.id);

    let switch = s.switch("Save transcript");
    assert_eq!(switch.title(), "Save transcript");
    assert!(!switch.is_active());
    switch.set_active(true);
    saved();
    assert!(s.on_disk(&p.id).expect("there").transcript);

    let port: adw::SpinRow = find_widget(&s.window).expect("the port row");
    port.set_value(2222.0);
    pump_for(Duration::from_millis(300));
    assert_eq!(s.on_disk(&p.id).expect("there").port, 22, "not settled yet");
    pump_until("the port to be saved", || {
        s.on_disk(&p.id).expect("there").port == 2222
    });
    // Leaving the row saves at once.
    port.grab_focus();
    port.set_value(2200.0);
    s.entry("Host").grab_focus();
    saved();
    assert_eq!(s.on_disk(&p.id).expect("there").port, 2200);

    let key_row = all_widgets::<adw::ActionRow>(&s.window)
        .into_iter()
        .find(|r| r.title() == "Key file")
        .expect("the key file row");
    assert_eq!(
        key_row.subtitle().as_deref(),
        Some("/home/alice/.ssh/id_ed25519")
    );
    assert!(!key_row.uses_markup());
    button_labeled(&key_row, "Clear")
        .expect("the clear button")
        .emit_clicked();
    saved();
    assert_eq!(s.on_disk(&p.id).expect("there").key_file, None);
    assert_eq!(key_row.subtitle().as_deref(), Some("None"));
    s.close();
}

pub(super) fn a_settling_port_is_saved_when_the_page_goes(dir: &Path) {
    let p = web();
    let q = Profile::new("nas", "nas.lan", "admin");

    // Another profile selected before the port settled.
    let s = Setup::open(dir, &[p.clone(), q.clone()]);
    s.select(&p.id);
    let port: adw::SpinRow = find_widget(&s.window).expect("the port row");
    port.grab_focus();
    port.set_value(2201.0);
    pump_for(Duration::from_millis(50));
    assert_eq!(s.on_disk(&p.id).expect("there").port, 22, "still settling");
    s.select(&q.id);
    saved();
    assert_eq!(s.on_disk(&p.id).expect("there").port, 2201);
    pump_for(Duration::from_millis(100));
    assert_eq!(s.entry("Host").text(), q.host, "q's page is shown");
    assert!(
        s.rows()
            .iter()
            .any(|r| r.subtitle().as_deref() == Some("alice@web.example.test:2201")),
        "the sidebar follows"
    );

    // The window closed before the port settled.
    s.select(&p.id);
    let port: adw::SpinRow = find_widget(&s.window).expect("the port row");
    port.grab_focus();
    port.set_value(2202.0);
    pump_for(Duration::from_millis(50));
    assert_eq!(
        s.on_disk(&p.id).expect("there").port,
        2201,
        "still settling"
    );
    let dir = s.dir.clone();
    s.close();
    saved();
    let stored = ProfileStore::open(&dir.join("profiles.json"))
        .expect("store")
        .get(&p.id)
        .cloned()
        .expect("there");
    assert_eq!(stored.port, 2202);
}

pub(super) fn refresh_keeps_an_unapplied_edit(dir: &Path) {
    let p = web();
    let s = Setup::open(dir, std::slice::from_ref(&p));
    s.select(&p.id);
    let name = s.entry("Name");
    name.grab_focus();
    name.set_text("draft");

    let mut changed = p.clone();
    changed.host = "moved.example.test".to_owned();
    s.other_store().upsert(changed).expect("save");
    s.ctx.profiles_changed();
    pump_for(Duration::from_millis(50));

    assert_eq!(s.entry("Host").text(), "moved.example.test");
    assert_eq!(name.text(), "draft");
    // Leaving now goes back to the stored name, not to the old snapshot.
    s.entry("Host").grab_focus();
    pump_for(Duration::from_millis(50));
    assert_eq!(name.text(), p.name);
    s.close();
}

pub(super) fn deleted_by_sync_shows_the_empty_state(dir: &Path) {
    let p = web();
    let keep = Profile::new("keep", "keep.lan", "bob");
    let s = Setup::open(dir, &[p.clone(), keep.clone()]);
    s.select(&p.id);
    assert!(!s.shows_empty_state());

    s.other_store().delete(&p.id).expect("delete");
    s.ctx.profiles_changed();
    pump_for(Duration::from_millis(100));

    assert!(s.shows_empty_state());
    assert!(s.rows().iter().all(|r| r.widget_name() != p.id.as_str()));
    assert_eq!(s.row_titles(), ["keep"]);
    assert!(s.sidebar().selected_row().is_none());
    s.close();
}

pub(super) fn the_file_watch_refreshes_the_window(dir: &Path) {
    let p = web();
    let s = Setup::open(dir, std::slice::from_ref(&p));
    s.ctx.watch_profile_file();
    s.select(&p.id);

    // Another process writes the file; nobody calls `profiles_changed`.
    let added = Profile::new("added", "added.lan", "bob");
    let mut changed = p.clone();
    changed.host = "watched.example.test".to_owned();
    let mut other = s.other_store();
    other.upsert(added).expect("save");
    other.upsert(changed).expect("save");

    pump_until("the window to follow the file", || {
        s.row_titles() == ["added", "web"] && s.entry("Host").text() == "watched.example.test"
    });
    s.close();
}

pub(super) fn double_click_and_connect_button_connect(dir: &Path) {
    let mut p = web();
    p.transcript = true;
    let q = Profile::new("nas", "nas.lan", "admin");
    let s = Setup::open(dir, &[p.clone(), q.clone()]);

    // A single click (selection) connects nothing.
    s.select(&q.id);
    assert!(s.connected.borrow().is_empty());

    let row = s
        .rows()
        .into_iter()
        .find(|r| r.widget_name() == p.id.as_str())
        .expect("p's row");
    s.sidebar().emit_by_name::<()>("row-activated", &[&row]);
    assert_eq!(
        *s.connected.borrow(),
        [Opened::Profile(p.id.clone(), None, true)]
    );

    let connect = button_labeled(&s.window, "Connect").expect("the Connect button");
    assert!(connect.has_css_class("suggested-action"));
    connect.emit_clicked();
    assert_eq!(
        s.connected.borrow().last(),
        Some(&Opened::Profile(q.id.clone(), None, false))
    );

    // "Save transcripts of all sessions" covers this way in too: q does not
    // record by itself, and is recorded now.
    crate::settings::update(&s.ctx.settings_path, |x| x.save_all_transcripts = true)
        .expect("write the settings");
    s.sidebar().emit_by_name::<()>(
        "row-activated",
        &[&s.rows()
            .into_iter()
            .find(|r| r.widget_name() == q.id.as_str())
            .expect("q's row")],
    );
    assert_eq!(
        s.connected.borrow().last(),
        Some(&Opened::Profile(q.id.clone(), None, true))
    );
    s.close();
}

pub(super) fn new_profile_dialog_creates_and_selects(dir: &Path) {
    let s = Setup::open(dir, &[web()]);
    let add = all_widgets::<gtk::Button>(&s.window)
        .into_iter()
        .find(|b| b.tooltip_text().as_deref() == Some("Add Profile"))
        .expect("the + button");
    add.emit_clicked();
    pump_for(Duration::from_millis(200));
    let dialog = s.window.visible_dialog().expect("the New Profile dialog");
    let row = |title: &str| {
        all_widgets::<adw::EntryRow>(&dialog)
            .into_iter()
            .find(|r| r.title() == title)
            .unwrap_or_else(|| panic!("no {title:?} row"))
    };
    let create = button_labeled(&dialog, "Create").expect("the Create button");
    assert!(!create.is_sensitive());
    row("Name").set_text("NAS");
    row("Host").set_text("nas.lan");
    assert!(!create.is_sensitive(), "no username yet");
    row("Username").set_text("admin");
    assert!(create.is_sensitive());
    row("Host").set_text("  ");
    assert!(!create.is_sensitive(), "a blank host");
    row("Host").set_text("nas.lan");
    row("Password (optional)").set_text("nas-password");
    create.emit_clicked();
    pump_for(Duration::from_millis(300));

    assert!(s.window.visible_dialog().is_none(), "the dialog closed");
    let id = s
        .ctx
        .store
        .borrow()
        .live()
        .iter()
        .find(|p| p.name == "NAS")
        .map(|p| p.id.clone())
        .expect("the new profile");
    assert_eq!(s.secrets.password(&id).as_deref(), Some("nas-password"));
    let selected = s.sidebar().selected_row().expect("a selection");
    assert_eq!(selected.widget_name(), id.as_str());
    assert_eq!(s.entry("Host").text(), "nas.lan");
    assert_eq!(
        row("Password (optional)").text(),
        "",
        "no password left in the closed dialog"
    );
    s.close();
}

pub(super) fn delete_asks_then_removes_profile_and_password(dir: &Path) {
    let mut p = web();
    p.name = MARKUP_NAME.to_owned();
    let s = Setup::open(dir, std::slice::from_ref(&p));
    s.secrets.set(&password_key(&p.id), "pw").expect("set");
    s.select(&p.id);

    button_labeled(&s.window, "Delete Profile")
        .expect("the delete button")
        .emit_clicked();
    pump_for(Duration::from_millis(200));
    let dialog = s
        .window
        .visible_dialog()
        .and_downcast::<adw::AlertDialog>()
        .expect("the delete confirmation");
    assert_eq!(
        dialog.heading().as_deref(),
        Some("Delete \u{201c}<b>x</b> &amp;\u{201d}?")
    );
    assert!(!dialog.is_heading_use_markup());
    // Nothing happens before the answer.
    assert!(s.on_disk(&p.id).is_some());
    button_labeled(&dialog, "Delete")
        .expect("the dialog's Delete button")
        .emit_clicked();
    pump_for(Duration::from_millis(300));

    assert!(s.on_disk(&p.id).is_none());
    assert_eq!(s.secrets.password(&p.id), None);
    assert!(s.rows().is_empty());
    assert!(s.shows_empty_state());
    s.close();
}

pub(super) fn the_page_says_password_removal_is_local(dir: &Path) {
    let p = web();
    let s = Setup::open(dir, std::slice::from_ref(&p));
    s.secrets.set(&password_key(&p.id), "pw").expect("set");
    s.select(&p.id);

    let row = all_widgets::<adw::ActionRow>(&s.window)
        .into_iter()
        .find(|r| r.title() == form::REMOVE_PASSWORD_LABEL)
        .expect("the remove row");
    assert_eq!(
        form::REMOVE_PASSWORD_LABEL,
        "Remove stored password (this machine only)"
    );
    assert_eq!(row.subtitle().as_deref(), Some(form::REMOVE_PASSWORD_NOTE));

    // Remove asks first, then deletes the keyring entry only.
    let before = s.file_bytes();
    button_labeled(&row, "Remove")
        .expect("the remove button")
        .emit_clicked();
    pump_for(Duration::from_millis(200));
    let dialog = s
        .window
        .visible_dialog()
        .and_downcast::<adw::AlertDialog>()
        .expect("the remove confirmation");
    assert!(dialog.body().contains(form::REMOVE_PASSWORD_NOTE));
    assert_eq!(s.secrets.password(&p.id).as_deref(), Some("pw"));
    button_labeled(&dialog, "Remove")
        .expect("the dialog's Remove button")
        .emit_clicked();
    pump_for(Duration::from_millis(300));
    assert_eq!(s.secrets.password(&p.id), None);
    assert_eq!(s.file_bytes(), before, "removal is not synced");

    // The secrets warnings are on the page.
    let groups = all_widgets::<adw::PreferencesGroup>(&s.window);
    assert!(
        groups
            .iter()
            .any(|g| g.description().as_deref() == Some(profiles_page_warning()))
    );
    s.close();
}

fn profiles_page_warning() -> &'static str {
    "Startup commands are saved as plain text in the profile file, and a transcript \
     records whatever appears in the terminal, including anything you type or paste. \
     Keep passwords out of both."
}

pub(super) fn sidebar_and_title_show_markup_literally(dir: &Path) {
    let mut p = Profile::new(MARKUP_NAME, "web.example.test", "alice");
    p.key_file = Some(PathBuf::from(format!("/k/{MARKUP_NAME}")));
    p.sequences = vec![moniker_core::profile::StartupSequence {
        name: MARKUP_NAME.to_owned(),
        commands: vec!["uptime".to_owned()],
    }];
    let s = Setup::open(dir, std::slice::from_ref(&p));

    let row = &s.rows()[0];
    assert_eq!(row.title(), MARKUP_NAME);
    assert!(!row.uses_markup());

    s.select(&p.id);
    let title = all_widgets::<gtk::Label>(&s.window)
        .into_iter()
        .find(|l| l.widget_name() == profiles::TITLE_NAME)
        .expect("the page title");
    assert_eq!(title.label(), MARKUP_NAME);
    assert!(!title.uses_markup());
    let name = s.entry("Name");
    assert_eq!(name.text(), MARKUP_NAME);
    assert!(!name.uses_markup());

    for (title, subtitle) in [
        (MARKUP_NAME.to_owned(), Some("1 command".to_owned())),
        ("Key file".to_owned(), Some(format!("/k/{MARKUP_NAME}"))),
    ] {
        let row = all_widgets::<adw::ActionRow>(&s.window)
            .into_iter()
            .find(|r| r.title() == title.as_str() && r.subtitle().map(String::from) == subtitle)
            .unwrap_or_else(|| panic!("no row {title:?} / {subtitle:?}"));
        assert!(!row.uses_markup(), "{title}");
    }
    s.close();
}

pub(super) fn add_edit_move_delete_a_sequence(dir: &Path) {
    let p = web();
    let s = Setup::open(dir, std::slice::from_ref(&p));
    s.select(&p.id);
    assert_eq!(
        s.sequence_row_titles(),
        ["None: connecting gives a plain shell", "Add sequence"]
    );

    // Add: the Add sequence row opens an empty sub-page with no order or
    // delete buttons (there is nothing yet to move or delete).
    let page = s.open_sequence("Add sequence");
    assert_eq!(sub_name(&page).text(), "");
    assert_eq!(commands_text(&page), "");
    for label in ["Move Up", "Move Down", "Delete Sequence"] {
        assert!(
            button_labeled(&page, label).is_none_or(|b| !b.is_mapped()),
            "{label} on a new sequence"
        );
    }
    sub_name(&page).set_text("update");
    set_commands(&page, " sudo apt update \n\n  df -h\n");
    s.click(&page, "Save");
    s.wait_for_sub_page_to_close();
    assert_eq!(
        s.stored_sequences(&p.id),
        [stored("update", &["sudo apt update", "df -h"])]
    );
    assert_eq!(s.sequence_row_titles(), ["update", "Add sequence"]);
    assert_eq!(
        s.sequence_rows()[0].subtitle().as_deref(),
        Some("2 commands")
    );

    let page = s.open_sequence("Add sequence");
    sub_name(&page).set_text("dmesg");
    set_commands(&page, "dmesg -T");
    s.click(&page, "Save");
    s.wait_for_sub_page_to_close();
    assert_eq!(s.sequence_row_titles(), ["update", "dmesg", "Add sequence"]);

    // Edit: reopen, see what was saved, change it.
    let page = s.open_sequence("update");
    assert_eq!(sub_name(&page).text(), "update");
    assert_eq!(commands_text(&page), "sudo apt update\ndf -h");
    assert_eq!(
        order_description(&page).as_deref(),
        Some("The connect options list sequences in this order.")
    );
    // First in the list: Move Up has nowhere to go.
    let up = button_labeled(&page, "Move Up").expect("Move Up");
    let down = button_labeled(&page, "Move Down").expect("Move Down");
    assert!(up.is_mapped() && down.is_mapped());
    assert!(!up.is_sensitive());
    assert!(down.is_sensitive());
    set_commands(&page, "sudo apt update\nsudo apt upgrade -y");
    s.click(&page, "Save");
    s.wait_for_sub_page_to_close();
    assert_eq!(
        s.stored_sequences(&p.id),
        [
            stored("update", &["sudo apt update", "sudo apt upgrade -y"]),
            stored("dmesg", &["dmesg -T"]),
        ]
    );
    assert_eq!(
        s.sequence_rows()[0].subtitle().as_deref(),
        Some("2 commands")
    );
    assert_eq!(
        s.sequence_rows()[1].subtitle().as_deref(),
        Some("1 command")
    );

    // Move: the second goes up. The page stays open, and text typed but not
    // saved is neither saved nor lost.
    let page = s.open_sequence("dmesg");
    let up = button_labeled(&page, "Move Up").expect("Move Up");
    let down = button_labeled(&page, "Move Down").expect("Move Down");
    assert!(up.is_sensitive());
    assert!(!down.is_sensitive(), "last in the list");
    set_commands(&page, "dmesg -T\nuptime");
    s.click(&page, "Move Up");
    assert_eq!(
        s.stored_sequences(&p.id),
        [
            stored("dmesg", &["dmesg -T"]),
            stored("update", &["sudo apt update", "sudo apt upgrade -y"]),
        ]
    );
    assert!(s.sequence_page().is_some(), "still on the sub-page");
    assert_eq!(commands_text(&page), "dmesg -T\nuptime");
    assert!(!up.is_sensitive(), "now first");
    assert!(down.is_sensitive());
    assert_eq!(s.sequence_row_titles(), ["dmesg", "update", "Add sequence"]);
    // Moving it back down works from its new place; Cancel drops the text.
    s.click(&page, "Move Down");
    assert_eq!(
        s.stored_sequences(&p.id)
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["update", "dmesg"]
    );
    s.click(&page, "Cancel");
    s.wait_for_sub_page_to_close();
    assert_eq!(
        s.stored_sequences(&p.id)[1],
        stored("dmesg", &["dmesg -T"]),
        "the unsaved text was not saved"
    );

    // Delete: gone, and the page closes.
    let page = s.open_sequence("update");
    s.click(&page, "Delete Sequence");
    s.wait_for_sub_page_to_close();
    assert_eq!(s.stored_sequences(&p.id), [stored("dmesg", &["dmesg -T"])]);
    assert_eq!(s.sequence_row_titles(), ["dmesg", "Add sequence"]);
    s.close();
}

pub(super) fn cancel_discards_sequence_edits(dir: &Path) {
    let p = web_with(&[seq("update", &["sudo apt update"])]);
    let s = Setup::open(dir, std::slice::from_ref(&p));
    s.select(&p.id);
    let before = s.file_bytes();

    let page = s.open_sequence("update");
    sub_name(&page).set_text("renamed");
    set_commands(&page, "something else");
    s.click(&page, "Cancel");
    s.wait_for_sub_page_to_close();
    assert_eq!(s.file_bytes(), before);
    let page = s.open_sequence("update");
    assert_eq!(sub_name(&page).text(), "update");
    assert_eq!(commands_text(&page), "sudo apt update");

    // A new sequence that is canceled adds nothing.
    s.click(&page, "Cancel");
    s.wait_for_sub_page_to_close();
    let page = s.open_sequence("Add sequence");
    sub_name(&page).set_text("new");
    set_commands(&page, "uptime");
    s.click(&page, "Cancel");
    s.wait_for_sub_page_to_close();
    assert_eq!(s.file_bytes(), before);
    assert_eq!(s.sequence_row_titles(), ["update", "Add sequence"]);

    // Any other way back (Escape, a swipe) is the same as Cancel.
    let page = s.open_sequence("update");
    sub_name(&page).set_text("renamed");
    assert!(s.content_nav().pop());
    s.wait_for_sub_page_to_close();
    assert_eq!(s.file_bytes(), before);
    s.close();
}

pub(super) fn sequence_errors_stay_on_the_sub_page(dir: &Path) {
    let p = web();
    let s = Setup::open(dir, std::slice::from_ref(&p));
    s.select(&p.id);
    let before = s.file_bytes();
    let page = s.open_sequence("Add sequence");

    // No commands left after blank lines are dropped.
    sub_name(&page).set_text("update");
    set_commands(&page, "  \n \n");
    s.click(&page, "Save");
    let message = "A startup sequence needs at least one command.";
    assert_eq!(inline_error(&page).as_deref(), Some(message));
    assert_eq!(s.ctx.notice.last_text().as_deref(), Some(message));
    assert_eq!(s.file_bytes(), before);
    assert!(s.sequence_page().is_some(), "still on the sub-page");
    assert_eq!(sub_name(&page).text(), "update", "what was typed is kept");

    // Editing clears the message.
    set_commands(&page, "uptime");
    assert_eq!(inline_error(&page), None);

    // A blank name is refused by the profile's own validation. The command
    // typed with it is not in any message.
    sub_name(&page).set_text("   ");
    set_commands(&page, "echo s3cret-token");
    s.click(&page, "Save");
    let message = "Every startup sequence needs a name, without control characters.";
    assert_eq!(inline_error(&page).as_deref(), Some(message));
    assert_eq!(s.ctx.notice.last_text().as_deref(), Some(message));
    assert_eq!(s.file_bytes(), before);
    assert_eq!(commands_text(&page), "echo s3cret-token");
    assert!(commands_view(&page).is_editable() && sub_name(&page).is_editable());
    assert!(page.can_pop(), "can be left again after a failed save");
    for text in [inline_error(&page), s.ctx.notice.last_text()] {
        assert!(!text.expect("a message").contains("s3cret"));
    }
    assert!(
        all_widgets::<gtk::Label>(&page)
            .iter()
            .all(|label| !label.label().contains("s3cret")),
        "no label on the page quotes a command"
    );

    // Fixed, it saves.
    sub_name(&page).set_text("show");
    s.click(&page, "Save");
    s.wait_for_sub_page_to_close();
    assert_eq!(
        s.stored_sequences(&p.id),
        [stored("show", &["echo s3cret-token"])]
    );
    s.close();
}

pub(super) fn sequence_rows_show_names_literally(dir: &Path) {
    let p = web_with(&[seq(MARKUP_NAME, &["echo <b>hi</b> &amp;"])]);
    let s = Setup::open(dir, std::slice::from_ref(&p));
    s.select(&p.id);

    let rows = s.sequence_rows();
    assert_eq!(rows[0].title(), MARKUP_NAME);
    assert_eq!(rows[0].subtitle().as_deref(), Some("1 command"));
    assert!(rows.iter().all(|row| !row.uses_markup()));

    let page = s.open_sequence(MARKUP_NAME);
    assert_eq!(sub_name(&page).text(), MARKUP_NAME);
    assert!(!sub_name(&page).uses_markup());
    assert_eq!(commands_text(&page), "echo <b>hi</b> &amp;");
    // Only libadwaita's own group headings parse markup (static text here);
    // nothing that shows the sequence's data does.
    assert!(
        all_widgets::<gtk::Label>(&page)
            .iter()
            .filter(|label| label.label().contains('<') || label.label().contains('&'))
            .all(|label| !label.uses_markup()),
        "a label with the sequence's markup characters parses them"
    );

    // Saved and shown back exactly as typed.
    let renamed = format!("{MARKUP_NAME} two");
    sub_name(&page).set_text(&renamed);
    s.click(&page, "Save");
    s.wait_for_sub_page_to_close();
    assert_eq!(s.sequence_rows()[0].title(), renamed);
    assert_eq!(
        s.stored_sequences(&p.id),
        [stored(&renamed, &["echo <b>hi</b> &amp;"])]
    );
    s.close();
}

pub(super) fn a_refresh_keeps_the_sub_pages_text(dir: &Path) {
    let p = web_with(&[seq("update", &["sudo apt update"])]);
    let s = Setup::open(dir, std::slice::from_ref(&p));
    s.select(&p.id);
    let page = s.open_sequence("update");
    sub_name(&page).set_text("update two");
    set_commands(&page, "one\ntwo");

    // A sync changes another field while the sub-page is open.
    let mut changed = p.clone();
    changed.host = "moved.example.test".to_owned();
    s.other_store().upsert(changed).expect("save");
    s.ctx.profiles_changed();
    pump_for(Duration::from_millis(100));

    assert!(s.sequence_page().is_some());
    assert_eq!(sub_name(&page).text(), "update two");
    assert_eq!(commands_text(&page), "one\ntwo");
    // Saving keeps the sync's change to the other field.
    s.click(&page, "Save");
    s.wait_for_sub_page_to_close();
    let stored_profile = s.on_disk(&p.id).expect("there");
    assert_eq!(stored_profile.host, "moved.example.test");
    assert_eq!(
        s.stored_sequences(&p.id),
        [stored("update two", &["one", "two"])]
    );
    assert_eq!(s.entry("Host").text(), "moved.example.test");
    s.close();
}

pub(super) fn saving_after_the_profile_was_deleted_fails_cleanly(dir: &Path) {
    let p = web();
    let keep = Profile::new("keep", "keep.lan", "bob");
    let s = Setup::open(dir, &[p.clone(), keep.clone()]);
    s.select(&p.id);
    let page = s.open_sequence("Add sequence");
    sub_name(&page).set_text("update");
    set_commands(&page, "sudo apt update");

    // A sync deletes the profile under the open sub-page. It stays, with the
    // person's text.
    s.other_store().delete(&p.id).expect("delete");
    s.ctx.profiles_changed();
    pump_for(Duration::from_millis(100));
    assert!(s.sequence_page().is_some());
    assert_eq!(commands_text(&page), "sudo apt update");
    let before = s.file_bytes();

    s.click(&page, "Save");
    assert_eq!(inline_error(&page).as_deref(), Some(form::GONE));
    assert_eq!(s.ctx.notice.last_text().as_deref(), Some(form::GONE));
    assert_eq!(s.file_bytes(), before, "nothing was written");
    assert!(s.on_disk(&p.id).is_none(), "the profile did not come back");
    assert!(s.sequence_page().is_some());
    assert_eq!(commands_text(&page), "sudo apt update");

    // Cancel leaves for the empty state.
    s.click(&page, "Cancel");
    s.wait_for_sub_page_to_close();
    assert!(s.shows_empty_state());
    s.close();
}

pub(super) fn a_sequence_changed_elsewhere_is_not_overwritten(dir: &Path) {
    let a = seq("a", &["1"]);
    let b = seq("b", &["2"]);
    let p = web_with(&[a.clone(), b.clone()]);
    let s = Setup::open(dir, std::slice::from_ref(&p));
    s.select(&p.id);
    let page = s.open_sequence("a");
    set_commands(&page, "edited");

    // A sync puts "b" first while the page is open on "a" (index 0).
    let mut reordered = p.clone();
    reordered.sequences = vec![b.clone(), a.clone()];
    s.other_store().upsert(reordered).expect("save");
    let before = s.file_bytes();

    for label in ["Save", "Move Down", "Delete Sequence"] {
        s.click(&page, label);
        assert_eq!(
            inline_error(&page).as_deref(),
            Some(form::SEQUENCE_CHANGED),
            "{label}"
        );
        assert_eq!(s.file_bytes(), before, "{label} wrote");
        assert!(s.sequence_page().is_some(), "{label} left the page");
    }
    assert_eq!(commands_text(&page), "edited");
    s.click(&page, "Cancel");
    s.wait_for_sub_page_to_close();
    assert_eq!(s.sequence_row_titles(), ["b", "a", "Add sequence"]);
    s.close();
}

// ----- the default sequence (issue #12) -----

/// The strings a combo row offers, in order.
fn items_of(row: &adw::ComboRow) -> Vec<String> {
    let model = row.model().expect("the row's list");
    (0..model.n_items())
        .map(|i| {
            model
                .item(i)
                .and_downcast::<gtk::StringObject>()
                .expect("a string")
                .string()
                .to_string()
        })
        .collect()
}

/// The string the combo row has selected.
fn shown_item(row: &adw::ComboRow) -> String {
    row.selected_item()
        .and_downcast::<gtk::StringObject>()
        .map(|item| item.string().to_string())
        .unwrap_or_default()
}

/// The default sequence is chosen in a combo row of the profile page, after the
/// sequences and before the row that adds one. Picking saves it by position,
/// and it follows its sequence when that moves, is renamed or goes (and
/// disappears with the last one); Connect passes it on.
pub(super) fn the_default_is_chosen_on_the_profile_page(dir: &Path) {
    let p = web_with(&[
        seq("a", &["echo A"]),
        seq("b", &["echo B"]),
        seq("c", &["echo C"]),
    ]);
    let s = Setup::open(dir, std::slice::from_ref(&p));
    s.select(&p.id);

    // What it offers and where it is.
    let combo = s.default_row();
    assert!(combo.is_mapped());
    assert_eq!(combo.title(), "Default sequence");
    assert_eq!(
        combo.subtitle().as_deref(),
        Some("Runs every time this profile connects")
    );
    assert!(!combo.uses_markup());
    assert_eq!(items_of(&combo), ["None", "a", "b", "c"]);
    assert_eq!(shown_item(&combo), "None");
    assert_eq!(
        s.group_row_titles(),
        ["a", "b", "c", "Default sequence", "Add sequence"]
    );
    assert_eq!(s.sequence_row_titles(), ["a", "b", "c", "Add sequence"]);
    assert_eq!(s.stored_default(&p.id), None);

    // Picking saves its place in the list, and Connect passes it on.
    s.pick_default("b");
    assert_eq!(s.stored_default(&p.id), Some(1));
    assert_eq!(shown_item(&combo), "b");
    assert!(s.on_disk(&p.id).unwrap().updated_at > p.updated_at);
    button_labeled(&s.window, "Connect")
        .expect("the Connect button")
        .emit_clicked();
    assert_eq!(
        *s.connected.borrow(),
        [Opened::Profile(p.id.clone(), Some(1), false)]
    );

    // Its sequence moves up: the default goes with it, in the same write.
    let page = s.open_sequence("b");
    s.click(&page, "Move Up");
    assert_eq!(
        s.stored_sequences(&p.id)
            .iter()
            .map(|(n, _)| n.as_str())
            .collect::<Vec<_>>(),
        ["b", "a", "c"]
    );
    assert_eq!(s.stored_default(&p.id), Some(0));
    assert_eq!(s.default_row(), combo, "the same row");
    assert_eq!(items_of(&combo), ["None", "b", "a", "c"]);
    assert_eq!(shown_item(&combo), "b", "still on b");

    // Deleting the default's sequence clears it.
    s.click(&page, "Delete Sequence");
    s.wait_for_sub_page_to_close();
    assert_eq!(s.stored_default(&p.id), None);
    assert_eq!(items_of(&combo), ["None", "a", "c"]);
    assert_eq!(shown_item(&combo), "None");

    // Deleting one before it moves it up one place, and it stays on its own.
    s.pick_default("c");
    assert_eq!(s.stored_default(&p.id), Some(1));
    let page = s.open_sequence("a");
    s.click(&page, "Delete Sequence");
    s.wait_for_sub_page_to_close();
    assert_eq!(s.stored_default(&p.id), Some(0));
    assert_eq!(items_of(&combo), ["None", "c"]);
    assert_eq!(shown_item(&combo), "c");

    // Renaming it changes the name on offer, not the choice.
    let page = s.open_sequence("c");
    sub_name(&page).set_text("c2");
    s.click(&page, "Save");
    s.wait_for_sub_page_to_close();
    assert_eq!(s.stored_default(&p.id), Some(0));
    assert_eq!(items_of(&combo), ["None", "c2"]);
    assert_eq!(shown_item(&combo), "c2");

    // None is a choice too.
    s.pick_default("None");
    assert_eq!(s.stored_default(&p.id), None);
    assert_eq!(shown_item(&combo), "None");
    assert_eq!(s.on_disk(&p.id).unwrap().startup_choice(), None);

    // With no sequence left there is nothing to choose: the row goes.
    s.pick_default("c2");
    assert_eq!(s.stored_default(&p.id), Some(0));
    let page = s.open_sequence("c2");
    s.click(&page, "Delete Sequence");
    s.wait_for_sub_page_to_close();
    assert_eq!(s.stored_default(&p.id), None);
    assert!(!combo.is_mapped(), "no default to choose");
    assert_eq!(
        s.group_row_titles(),
        ["None: connecting gives a plain shell", "Add sequence"]
    );

    // A new sequence brings it back, with nothing chosen.
    let page = s.open_sequence("Add sequence");
    sub_name(&page).set_text("update");
    set_commands(&page, "sudo apt update");
    s.click(&page, "Save");
    s.wait_for_sub_page_to_close();
    assert!(combo.is_mapped());
    assert_eq!(items_of(&combo), ["None", "update"]);
    assert_eq!(shown_item(&combo), "None");
    assert_eq!(s.stored_default(&p.id), None);
    s.close();
}

/// The row shows what the file says, and only a pick saves: building the page,
/// a refresh, a sequence added or taken away elsewhere write nothing, even when
/// they rebuild the list on offer. A pick that cannot be saved goes back to the
/// file's choice and says why. A pick is refused when the list changed under it
/// (reordered, shortened) and the page has not heard of it yet: the default is
/// kept by position, and it must never name a sequence the person did not
/// choose.
pub(super) fn the_default_row_follows_the_file_and_saves_only_a_pick(dir: &Path) {
    let mut p = web_with(&[
        seq("a", &["echo A"]),
        seq("b", &["echo B"]),
        seq("c", &["echo C"]),
    ]);
    p.startup_sequence = Some(2);
    let s = Setup::open(dir, std::slice::from_ref(&p));
    let first = s.file_bytes();
    s.select(&p.id);
    let combo = s.default_row();
    assert_eq!(shown_item(&combo), "c", "what the file says");
    let idle = |what: &str, written: &[u8]| {
        pump_for(Duration::from_millis(300));
        assert_eq!(keyring::pending(), 0, "{what}: a save was asked for");
        assert_eq!(s.file_bytes(), written, "{what}: the file was written");
    };
    idle("building the page", &first);

    // The default changed elsewhere.
    s.elsewhere(&p.id, |p| p.startup_sequence = Some(0));
    let written = s.file_bytes();
    s.ctx.profiles_changed();
    assert_eq!(shown_item(&combo), "a");
    idle("a refresh", &written);

    // A sequence added elsewhere: the list on offer is made again, the choice
    // stays, nothing is saved.
    s.elsewhere(&p.id, |p| p.sequences.push(seq("d", &["echo D"])));
    let written = s.file_bytes();
    s.ctx.profiles_changed();
    assert_eq!(items_of(&combo), ["None", "a", "b", "c", "d"]);
    assert_eq!(shown_item(&combo), "a");
    idle("a longer list", &written);

    // The sequence it named taken away, with none left to name: nothing is
    // chosen, and the file keeps what it has.
    s.elsewhere(&p.id, |p| {
        p.sequences.truncate(1);
        p.startup_sequence = Some(2);
    });
    let written = s.file_bytes();
    s.ctx.profiles_changed();
    assert_eq!(items_of(&combo), ["None", "a"]);
    assert_eq!(shown_item(&combo), "None");
    idle("a default past the end", &written);
    assert_eq!(s.stored_default(&p.id), Some(2), "left as it was");

    // A pick that cannot be saved (the file cannot be replaced) goes back to
    // what the file says, with the reason.
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o500)).expect("chmod");
        let first_pick = items_of(&combo).iter().position(|i| i == "a").expect("a");
        combo.set_selected(u32::try_from(first_pick).expect("small"));
        saved();
        assert_eq!(shown_item(&combo), "None", "back to what is stored");
        assert!(combo.has_css_class("error"));
        assert!(
            s.ctx
                .notice
                .last_text()
                .is_some_and(|text| text.starts_with("Could not save the profile")),
            "{:?}",
            s.ctx.notice.last_text()
        );
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).expect("chmod back");
        assert_eq!(s.file_bytes(), written, "nothing was written");
    }
    // The same pick works now, and clears the mark.
    s.pick_default("a");
    assert_eq!(s.stored_default(&p.id), Some(0));
    assert!(!combo.has_css_class("error"));

    // The list was reordered elsewhere before the page heard of it (the file
    // watch has not reloaded: nothing ran the main loop since the write). The
    // list has the same length, so only the sequence the pick carries tells:
    // "a" at its old place is "b" now, and a pick of it would make every
    // connection run another sequence than the one the person saw. It is
    // refused, the file keeps its default, and the row goes back to what the
    // file says.
    s.elsewhere(&p.id, |p| {
        p.sequences = vec![
            seq("a", &["echo A"]),
            seq("b", &["echo B"]),
            seq("c", &["echo C"]),
        ];
        p.startup_sequence = Some(2);
    });
    s.ctx.profiles_changed();
    assert_eq!(items_of(&combo), ["None", "a", "b", "c"]);
    assert_eq!(shown_item(&combo), "c");
    s.elsewhere(&p.id, |p| {
        p.sequences = vec![
            seq("b", &["echo B"]),
            seq("a", &["echo A"]),
            seq("c", &["echo C"]),
        ];
    });
    let written = s.file_bytes();
    let first = items_of(&combo).iter().position(|i| i == "a").expect("a");
    combo.set_selected(u32::try_from(first).expect("small"));
    saved();
    assert_eq!(
        s.ctx.notice.last_text().as_deref(),
        Some(form::CHOICE_CHANGED)
    );
    assert_eq!(s.file_bytes(), written, "nothing was written");
    assert_eq!(s.stored_default(&p.id), Some(2), "the file's default");
    assert!(combo.has_css_class("error"));
    assert_eq!(items_of(&combo), ["None", "b", "a", "c"], "the file's list");
    assert_eq!(shown_item(&combo), "c", "the file's choice");

    // A list that was shortened elsewhere before the page heard of it: the
    // pick names a place the file no longer has, and is refused the same way.
    s.elsewhere(&p.id, |p| p.sequences.truncate(1));
    let written = s.file_bytes();
    let second = items_of(&combo).iter().position(|i| i == "a").expect("a");
    combo.set_selected(u32::try_from(second).expect("small"));
    saved();
    assert_eq!(
        s.ctx.notice.last_text().as_deref(),
        Some(form::CHOICE_CHANGED)
    );
    assert_eq!(s.file_bytes(), written, "nothing was written");
    assert_eq!(items_of(&combo), ["None", "b"], "the file's list now");
    assert_eq!(shown_item(&combo), "None", "its default is past the end");
    s.close();
}

pub(super) fn selecting_another_profile_closes_the_sub_page(dir: &Path) {
    let p = web_with(&[seq("update", &["sudo apt update"])]);
    let q = Profile::new("nas", "nas.lan", "admin");
    let s = Setup::open(dir, &[p.clone(), q.clone()]);
    s.select(&p.id);
    let page = s.open_sequence("update");
    sub_name(&page).set_text("typed");

    s.select(&q.id);
    s.wait_for_sub_page_to_close();
    assert_eq!(s.entry("Host").text(), q.host);
    s.select(&p.id);
    assert!(s.sequence_page().is_none(), "back on the profile page");
    assert_eq!(s.entry("Host").text(), p.host);
    assert_eq!(
        s.stored_sequences(&p.id),
        [stored("update", &["sudo apt update"])]
    );
    s.close();
}

pub(super) fn the_narrow_window_keeps_its_back_button(dir: &Path) {
    let p = web_with(&[seq("update", &["sudo apt update"])]);
    let s = Setup::open(dir, std::slice::from_ref(&p));
    let split = all_widgets::<adw::NavigationSplitView>(&s.window)
        .into_iter()
        .next()
        .expect("the split view");
    // Narrow: one pane at a time. The sub-page sits in a navigation view
    // inside the content pane, which must not take the profile page's back
    // button (to the list) away.
    split.set_collapsed(true);
    s.select(&p.id);
    pump_for(Duration::from_millis(300));
    let back_buttons = || {
        all_widgets::<gtk::Button>(&s.window)
            .into_iter()
            .filter(|b| b.has_css_class("back") && b.is_mapped())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        back_buttons().len(),
        1,
        "back to the list on the profile page"
    );

    // On the sub-page Cancel is the way out: there is no second back button.
    let page = s.open_sequence("update");
    pump_for(Duration::from_millis(300));
    assert!(back_buttons().is_empty());
    s.click(&page, "Cancel");
    s.wait_for_sub_page_to_close();

    let back = back_buttons().pop().expect("the back button");
    assert!(split.shows_content());
    back.emit_clicked();
    pump_for(Duration::from_millis(300));
    assert!(!split.shows_content(), "back to the list");
    s.close();
}

pub(super) fn present_reuses_the_open_window(dir: &Path) {
    let s = Setup::open(dir, &[web()]);
    let app = s.window.application().expect("an application");
    let app: adw::Application = app.downcast().expect("an adw application");
    let again = profiles::present(&app, &s.ctx, Rc::new(|_, _| {}));
    assert_eq!(again, s.window);
    s.close();
    pump_for(Duration::from_millis(100));
    assert!(app.windows().is_empty());
}

/// The Connection Profiles window calls the keyring on the keyring worker,
/// never on the main thread (issue #4): saving a password, removing the stored
/// one, creating a profile with a password and deleting a profile. Each shows
/// its outcome once the worker is back.
pub(super) fn the_profile_window_uses_the_keyring_off_the_main_thread(dir: &Path) {
    let p = web();
    let s = Setup::open(dir, std::slice::from_ref(&p));
    let before = keyring::main_thread_calls();
    let still_free = |what: &str| {
        assert_eq!(
            keyring::main_thread_calls(),
            before,
            "{what} called the keyring on the main thread"
        );
    };
    s.select(&p.id);

    let password = s.entry("Password");
    password.grab_focus();
    password.set_text("pw-one");
    apply(&password);
    pump_until("the password to be stored and the field cleared", || {
        s.secrets.password(&p.id).as_deref() == Some("pw-one") && password.text().is_empty()
    });
    still_free("saving a password");

    let remove_row = all_widgets::<adw::ActionRow>(&s.window)
        .into_iter()
        .find(|r| r.title() == form::REMOVE_PASSWORD_LABEL)
        .expect("the remove row");
    button_labeled(&remove_row, "Remove")
        .expect("the remove button")
        .emit_clicked();
    pump_until("the remove confirmation", || {
        s.window.visible_dialog().is_some()
    });
    let dialog = s.window.visible_dialog().expect("the remove confirmation");
    button_labeled(&dialog, "Remove")
        .expect("the dialog's Remove button")
        .emit_clicked();
    pump_until("the password to be removed", || {
        s.secrets.password(&p.id).is_none()
    });
    still_free("removing the stored password");

    let add = all_widgets::<gtk::Button>(&s.window)
        .into_iter()
        .find(|b| b.tooltip_text().as_deref() == Some("Add Profile"))
        .expect("the + button");
    add.emit_clicked();
    pump_until("the New Profile dialog", || {
        s.window.visible_dialog().is_some()
    });
    let dialog = s.window.visible_dialog().expect("the New Profile dialog");
    let field = |title: &str| {
        all_widgets::<adw::EntryRow>(&dialog)
            .into_iter()
            .find(|r| r.title() == title)
            .unwrap_or_else(|| panic!("no {title:?} row"))
    };
    field("Name").set_text("NAS");
    field("Host").set_text("nas.lan");
    field("Username").set_text("admin");
    field("Password (optional)").set_text("nas-password");
    button_labeled(&dialog, "Create")
        .expect("the Create button")
        .emit_clicked();
    let created = || {
        s.ctx
            .store
            .borrow()
            .live()
            .iter()
            .find(|p| p.name == "NAS")
            .map(|p| p.id.clone())
    };
    pump_until("the new profile", || {
        created().is_some() && s.window.visible_dialog().is_none()
    });
    let nas = created().expect("the new profile");
    assert_eq!(s.secrets.password(&nas).as_deref(), Some("nas-password"));
    still_free("creating a profile with a password");

    s.select(&nas);
    button_labeled(&s.window, "Delete Profile")
        .expect("the delete button")
        .emit_clicked();
    pump_until("the delete confirmation", || {
        s.window.visible_dialog().is_some()
    });
    let dialog = s.window.visible_dialog().expect("the delete confirmation");
    button_labeled(&dialog, "Delete")
        .expect("the dialog's Delete button")
        .emit_clicked();
    pump_until("the profile and its password to go", || {
        s.on_disk(&nas).is_none() && s.secrets.password(&nas).is_none()
    });
    still_free("deleting a profile");
    s.close();
}

/// What the keyring worker says comes through even when the page that asked
/// has gone (issue #4): a password the keyring refused to store is told,
/// although the person moved on to another profile while it waited. And
/// deleting a profile is one step on the worker: a password saved for it while
/// the deletion waits for the keyring is not kept for a profile that is gone.
pub(super) fn the_keyring_workers_answers_are_never_lost(dir: &Path) {
    let p = web();
    let q = Profile::new("nas", "nas.lan", "admin");
    let held = Rc::new(std::cell::RefCell::new(None::<Arc<HeldKeyring>>));
    let s = Setup::open_with(dir, &[p.clone(), q.clone()], |inner| {
        let keyring = Arc::new(HeldKeyring::new(inner));
        held.replace(Some(Arc::clone(&keyring)));
        keyring
    });
    let keyring = held.borrow().clone().expect("the held keyring");

    // A refused password, told after the page went.
    keyring.hold();
    keyring.refuse_changes(true);
    s.select(&p.id);
    let password = s.entry("Password");
    password.grab_focus();
    password.set_text("pw-refused");
    press_apply(&password);
    s.select(&q.id);
    keyring.release();
    pump_until("the refusal to be told", || {
        s.ctx
            .notice
            .last_text()
            .is_some_and(|t| t.starts_with("Could not store the password in the keyring"))
    });
    assert_eq!(s.secrets.password(&p.id), None);
    keyring.refuse_changes(false);

    // A password saved while the profile is being deleted.
    keyring.hold();
    s.select(&p.id);
    button_labeled(&s.window, "Delete Profile")
        .expect("the delete button")
        .emit_clicked();
    pump_until("the delete confirmation", || {
        s.window.visible_dialog().is_some()
    });
    let dialog = s.window.visible_dialog().expect("the delete confirmation");
    button_labeled(&dialog, "Delete")
        .expect("the dialog's Delete button")
        .emit_clicked();
    pump_until("the confirmation to close", || {
        s.window.visible_dialog().is_none()
    });
    let password = s.entry("Password");
    password.grab_focus();
    password.set_text("pw-late");
    press_apply(&password);
    keyring.release();
    pump_until("the keyring calls to come back", || keyring::pending() == 0);
    assert!(s.on_disk(&p.id).is_none(), "the profile is deleted");
    assert_eq!(
        s.secrets.password(&p.id),
        None,
        "no password left for a profile that is gone"
    );
    s.close();
}

/// The profile file as a sync elsewhere would write it, with `change` made to
/// profile `id`: written in a copy beside it, so the file itself is untouched.
fn written_elsewhere(s: &Setup, id: &str, change: impl FnOnce(&mut Profile)) -> Vec<u8> {
    let copy = s.dir.join("elsewhere");
    fs::create_dir_all(&copy).expect("a folder for the copy");
    fs::copy(s.dir.join("profiles.json"), copy.join("profiles.json")).expect("copy");
    let mut store = ProfileStore::open(&copy.join("profiles.json")).expect("the copy");
    let mut profile = store.get(id).cloned().expect("the profile");
    change(&mut profile);
    store.upsert(profile).expect("save the copy");
    fs::read(copy.join("profiles.json")).expect("the copy's bytes")
}

/// Run `act`, which asks for a save while the profile file is held, and check
/// that it came back at once: a save that waited for the file on the main
/// thread would only come back when the lock is let go.
fn at_once(what: &str, act: impl FnOnce()) {
    let start = Instant::now();
    act();
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{what} waited for the profile file on the main thread"
    );
}

/// Whether a spinner shows in `root` (a row, or the sub-page).
fn spinner_shown(root: &impl IsA<gtk::Widget>) -> bool {
    all_widgets::<adw::Spinner>(root)
        .iter()
        .any(|spinner| spinner.is_visible())
}

/// Saving waits for the profile file, not the window (issue #7). With the
/// file's lock held, as a sync that waits for the keyring holds it, a text
/// field, the switch, the port, the key file and a startup sequence each ask
/// for their save at once and the window goes on. A row being saved keeps
/// the focus and what it shows (a refresh puts nothing back), takes no more
/// typing, and shows a spinner after a moment. Once the lock is let go the
/// saves are made in the order asked, and a failure is told even when its
/// page has gone.
pub(super) fn saving_waits_for_the_profile_file_not_the_window(dir: &Path) {
    let mut p = web_with(&[
        seq("update", &["sudo apt update"]),
        seq("reboot", &["sudo reboot"]),
    ]);
    p.key_file = Some(PathBuf::from("/home/alice/.ssh/id_ed25519"));
    let s = Setup::open(dir, std::slice::from_ref(&p));
    s.select(&p.id);
    let before = s.file_bytes();
    let held = HeldProfileFile::hold(dir);

    let name = s.entry("Name");
    name.grab_focus();
    name.set_text("  renamed ");
    at_once("applying the name", || {
        name.emit_by_name::<()>("apply", &[]);
    });
    assert!(!name.is_editable(), "no more typing while it saves");
    pump_until("the name row's spinner", || spinner_shown(&name));
    assert!(has_focus_in(&s.window, &name), "the row keeps the focus");
    assert_eq!(name.text(), "  renamed ");

    let switch = s.switch("Save transcript");
    at_once("turning the switch on", || switch.set_active(true));
    let port: adw::SpinRow = find_widget(&s.window).expect("the port row");
    at_once("changing the port", || port.set_value(2222.0));
    pump_until("the port row's spinner (once settled)", || {
        spinner_shown(&port)
    });
    let key_row = all_widgets::<adw::ActionRow>(&s.window)
        .into_iter()
        .find(|r| r.title() == "Key file")
        .expect("the key file row");
    at_once("clearing the key file", || {
        button_labeled(&key_row, "Clear")
            .expect("the clear button")
            .emit_clicked();
    });

    // A refresh meanwhile (a sync elsewhere, say) puts nothing back.
    s.ctx.profiles_changed();
    pump_for(Duration::from_millis(100));
    assert_eq!(name.text(), "  renamed ");
    assert!(switch.is_active());
    assert_eq!(port.value(), 2222.0);
    assert_eq!(key_row.subtitle().as_deref(), Some("None"));
    // A second change of the switch is a second save, after the first.
    at_once("turning the switch off again", || switch.set_active(false));
    assert_eq!(
        s.file_bytes(),
        before,
        "nothing written while the file is held"
    );

    held.release();
    pump_until("the saves", || keyring::pending() == 0);
    let stored_profile = s.on_disk(&p.id).expect("there");
    assert_eq!(stored_profile.name, "renamed");
    assert!(
        !stored_profile.transcript,
        "the later of the switch's saves"
    );
    assert_eq!(stored_profile.port, 2222);
    assert_eq!(stored_profile.key_file, None);
    assert_eq!(name.text(), "renamed", "the row shows what was stored");
    assert!(name.is_editable());
    assert!(!switch.is_active());
    assert_eq!(port.value(), 2222.0);
    assert_eq!(key_row.subtitle().as_deref(), Some("None"));
    for row in [
        name.upcast_ref::<gtk::Widget>(),
        port.upcast_ref(),
        switch.upcast_ref(),
        key_row.upcast_ref(),
    ] {
        assert!(!spinner_shown(row), "a spinner left on {}", row.type_());
        assert!(!row.has_css_class("error"));
    }
    assert_eq!(
        s.rows()[0].title().as_str(),
        "renamed",
        "the sidebar follows"
    );

    // A startup sequence: Save waits, the sub-page stays with its text, takes
    // no more typing and no second Save, and closes once saved.
    let page = s.open_sequence("update");
    set_commands(&page, "sudo apt update\nsudo apt upgrade");
    let held = HeldProfileFile::hold(dir);
    let save = button_labeled(&page, "Save").expect("the Save button");
    at_once("saving the sequence", || save.emit_clicked());
    at_once("pressing Save again", || save.emit_clicked());
    assert_eq!(keyring::pending(), 1, "one save");
    assert!(!commands_view(&page).is_editable());
    assert!(!sub_name(&page).is_editable());
    pump_until("the sub-page's spinner", || spinner_shown(&page));
    assert!(s.sequence_page().is_some(), "the sub-page waits");
    assert!(
        !button_labeled(&page, "Cancel")
            .expect("the Cancel button")
            .is_sensitive(),
        "Cancel waits for the save"
    );
    assert!(!page.can_pop(), "and so does Escape");
    held.release();
    s.wait_for_sub_page_to_close();
    assert_eq!(
        s.stored_sequences(&p.id),
        [
            stored("update", &["sudo apt update", "sudo apt upgrade"]),
            stored("reboot", &["sudo reboot"])
        ]
    );

    // Move Down waits too, and takes no second press; once it is back the
    // page is at the sequence's new place.
    let page = s.open_sequence("update");
    let held = HeldProfileFile::hold(dir);
    let down = button_labeled(&page, "Move Down").expect("Move Down");
    at_once("moving the sequence down", || down.emit_clicked());
    at_once("pressing it again", || down.emit_clicked());
    assert_eq!(keyring::pending(), 1, "one move");
    held.release();
    saved();
    assert_eq!(
        s.stored_sequences(&p.id),
        [
            stored("reboot", &["sudo reboot"]),
            stored("update", &["sudo apt update", "sudo apt upgrade"])
        ]
    );
    assert!(
        s.sequence_page().is_some(),
        "still open, on the moved sequence"
    );
    assert!(!down.is_sensitive(), "it is last now");
    assert!(
        button_labeled(&page, "Move Up")
            .expect("Move Up")
            .is_sensitive()
    );
    assert!(page.can_pop());

    // A sync that writes the file while a save waits for it is not written
    // over: the save is checked against what the sync wrote.
    set_commands(&page, "edited while a sync ran");
    let by_sync = written_elsewhere(&s, &p.id, |profile| {
        profile.sequences[1].name = "update (synced)".to_owned();
    });
    let held = HeldProfileFile::hold_then_write(dir, Some(by_sync.clone()));
    at_once("saving during the sync", || {
        button_labeled(&page, "Save")
            .expect("the Save button")
            .emit_clicked();
    });
    held.release();
    saved();
    assert_eq!(inline_error(&page).as_deref(), Some(form::SEQUENCE_CHANGED));
    assert_eq!(s.file_bytes(), by_sync, "what the sync wrote stays");
    assert_eq!(commands_text(&page), "edited while a sync ran");
    s.click(&page, "Cancel");
    s.wait_for_sub_page_to_close();

    // A save that fails after its page has gone is still told.
    let q = Profile::new("nas", "nas.lan", "admin");
    s.other_store().upsert(q.clone()).expect("save");
    s.ctx.profiles_changed();
    let held = HeldProfileFile::hold(dir);
    let host = s.entry("Host");
    host.grab_focus();
    host.set_text("bad host!");
    at_once("applying a bad host", || {
        host.emit_by_name::<()>("apply", &[]);
    });
    s.select(&q.id);
    held.release();
    pump_until("the failure to be told", || {
        s.ctx
            .notice
            .last_text()
            .is_some_and(|text| text.starts_with("Host must not be empty"))
    });
    assert_eq!(s.on_disk(&p.id).expect("there").host, p.host);
    s.close();
}

/// A save that fails puts the switch and the port back to what is stored (a
/// row that shows an unsaved state would mislead), marks them and says why.
pub(super) fn a_failed_save_puts_the_switch_and_the_port_back(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let p = web();
    let s = Setup::open(dir, std::slice::from_ref(&p));
    s.select(&p.id);
    // The file cannot be replaced (its folder is read-only): every save fails.
    fs::set_permissions(dir, fs::Permissions::from_mode(0o500)).expect("chmod");

    let switch = s.switch("Save transcript");
    switch.set_active(true);
    saved();
    assert!(!switch.is_active(), "back to what is stored");
    assert!(switch.has_css_class("error"));
    assert!(
        s.ctx
            .notice
            .last_text()
            .is_some_and(|text| text.starts_with("Could not save the profile")),
        "{:?}",
        s.ctx.notice.last_text()
    );

    let port: adw::SpinRow = find_widget(&s.window).expect("the port row");
    port.grab_focus();
    port.set_value(2222.0);
    s.entry("Host").grab_focus();
    saved();
    assert_eq!(port.value(), 22.0, "back to what is stored");
    assert!(port.has_css_class("error"));

    // The same for "Sync to other machines", after its question.
    let sync = s.switch("Sync to other machines");
    sync.set_active(false);
    button_labeled(&alert_in(&s.window), "Keep on This Machine Only")
        .expect("the Keep button")
        .emit_clicked();
    saved();
    pump_until("the row to be ready again", || sync.is_sensitive());
    assert!(sync.is_active(), "back to what is stored");
    assert!(sync.has_css_class("error"));
    assert!(
        s.ctx
            .notice
            .last_text()
            .is_some_and(|text| text.starts_with("Could not save the profile")),
        "{:?}",
        s.ctx.notice.last_text()
    );

    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).expect("chmod back");
    assert!(!s.on_disk(&p.id).expect("there").local_only);
    assert_eq!(s.on_disk(&p.id).expect("there").port, 22);
    s.close();
}

const SYNC_ROW: &str = "Sync to other machines";
const SYNCS_SUBTITLE: &str = "Copied to your other machines when they sync";
const KEPT_SUBTITLE: &str = "On this machine only";

/// The alert in `s`'s window is gone, and the profile page is ready again.
fn wait_for_the_alert_to_go(s: &Setup) {
    pump_until("the alert to go", || {
        all_widgets::<adw::AlertDialog>(&s.window).is_empty()
    });
}

/// The "Sync to other machines" switch of the profile page (issue #10). On
/// means the profile syncs, as every profile does; turning it off keeps the
/// profile on this machine only, which deletes it from the other machines at
/// their next sync, with its stored password, so the person is told first:
/// nothing is written until they say so, Cancel and Escape put the switch back,
/// and the row waits (insensitive) from the flip until the question is answered
/// and the save is back.
pub(super) fn keeping_a_profile_here_asks_first(dir: &Path) {
    let p = Profile::new("Box", "box.example.test", "alice");
    let s = Setup::open(dir, std::slice::from_ref(&p));
    s.select(&p.id);

    // A group of its own after Session; on, with a subtitle that says so.
    let groups = all_widgets::<adw::PreferencesGroup>(&s.window);
    let at = |title: &str| {
        groups
            .iter()
            .position(|g| g.title() == title)
            .unwrap_or_else(|| panic!("no {title:?} group"))
    };
    assert_eq!(at("Sync"), at("Session") + 1, "the Sync group follows");
    let row = s.switch(SYNC_ROW);
    assert!(
        all_widgets::<adw::SwitchRow>(&groups[at("Sync")]).contains(&row),
        "the switch is in the Sync group"
    );
    assert!(row.is_active(), "a profile syncs unless it is kept here");
    assert!(row.is_sensitive());
    assert!(!row.uses_markup());
    assert_eq!(row.subtitle().as_deref(), Some(SYNCS_SUBTITLE));
    let before = s.file_bytes();

    // Turning it off asks first, saying what will happen. Nothing is written.
    row.grab_focus();
    row.set_active(false);
    let alert = alert_in(&s.window);
    assert_eq!(
        alert.heading().as_deref(),
        Some("Keep \u{201c}Box\u{201d} on This Machine Only?")
    );
    assert!(!alert.is_heading_use_markup());
    assert_eq!(
        alert.body().as_str(),
        "It will be deleted from your other machines, with its stored password, the next time \
         they sync. It stays on this machine."
    );
    assert!(!alert.is_body_use_markup());
    assert_eq!(alert.default_response().as_deref(), Some("cancel"));
    assert_eq!(alert.close_response().as_str(), "cancel");
    assert_eq!(alert.response_label("cancel").as_str(), "Cancel");
    assert_eq!(
        alert.response_label("keep").as_str(),
        "Keep on This Machine Only"
    );
    assert_eq!(
        alert.response_appearance("keep"),
        adw::ResponseAppearance::Destructive
    );
    assert!(!row.is_sensitive(), "the row waits for the answer");
    pump_for(Duration::from_millis(300));
    assert_eq!(s.file_bytes(), before, "nothing written before the answer");
    assert_eq!(keyring::pending(), 0, "and no save asked for");
    // A refresh meanwhile (a sync elsewhere, say) leaves the row as asked.
    s.ctx.profiles_changed();
    pump_for(Duration::from_millis(100));
    assert!(!row.is_active(), "still showing the question's switch");

    // Cancel puts the switch back and writes nothing.
    button_labeled(&alert, "Cancel")
        .expect("the Cancel button")
        .emit_clicked();
    wait_for_the_alert_to_go(&s);
    assert!(row.is_active(), "back on");
    assert!(row.is_sensitive());
    pump_until("the keyboard to be back on the row", || {
        has_focus_in(&s.window, &row)
    });
    assert_eq!(row.subtitle().as_deref(), Some(SYNCS_SUBTITLE));
    pump_for(Duration::from_millis(200));
    assert_eq!(s.file_bytes(), before, "Cancel writes nothing");
    assert_eq!(keyring::pending(), 0);

    // Closing the alert (Escape) is Cancel.
    row.set_active(false);
    alert_in(&s.window).close();
    wait_for_the_alert_to_go(&s);
    assert!(row.is_active() && row.is_sensitive());
    assert_eq!(s.file_bytes(), before);

    // Keep on This Machine Only saves it, and the row waits for the save.
    row.grab_focus();
    row.set_active(false);
    let alert = alert_in(&s.window);
    button_labeled(&alert, "Keep on This Machine Only")
        .expect("the Keep button")
        .emit_clicked();
    pump_until("the file to say kept", || {
        s.on_disk(&p.id).is_some_and(|stored| stored.local_only)
    });
    saved();
    pump_until("the row to be ready again", || row.is_sensitive());
    pump_until(
        "the keyboard to be back on the row once it is saved",
        || has_focus_in(&s.window, &row),
    );
    let kept = s.on_disk(&p.id).expect("there");
    assert!(kept.local_only);
    assert!(kept.updated_at > p.updated_at, "an edit like any other");
    assert_eq!(kept.host, p.host, "nothing else changed");
    assert!(!row.is_active());
    assert_eq!(row.subtitle().as_deref(), Some(KEPT_SUBTITLE));
    assert!(!row.has_css_class("error"));
    assert!(!spinner_shown(&row));

    // A cancelled question after that goes back to what is saved now: kept.
    wait_for_the_alert_to_go(&s);
    let saved_bytes = s.file_bytes();
    row.set_active(true);
    let alert = alert_in(&s.window);
    assert_eq!(
        alert.heading().as_deref(),
        Some("Sync \u{201c}Box\u{201d} to Your Other Machines?")
    );
    button_labeled(&alert, "Cancel")
        .expect("the Cancel button")
        .emit_clicked();
    wait_for_the_alert_to_go(&s);
    assert!(!row.is_active() && row.is_sensitive());
    assert_eq!(row.subtitle().as_deref(), Some(KEPT_SUBTITLE));
    pump_for(Duration::from_millis(200));
    assert_eq!(s.file_bytes(), saved_bytes, "Cancel writes nothing");
    s.close();
}

/// Syncing a kept profile again copies it, password and all, to the other
/// machines, which cannot be called back, so that asks too (issue #10).
pub(super) fn syncing_it_again_asks_too(dir: &Path) {
    let mut p = Profile::new("Box", "box.example.test", "alice");
    p.local_only = true;
    let s = Setup::open(dir, std::slice::from_ref(&p));
    s.select(&p.id);
    let row = s.switch(SYNC_ROW);
    assert!(!row.is_active(), "kept here");
    assert_eq!(row.subtitle().as_deref(), Some(KEPT_SUBTITLE));
    let before = s.file_bytes();

    row.set_active(true);
    let alert = alert_in(&s.window);
    assert_eq!(
        alert.heading().as_deref(),
        Some("Sync \u{201c}Box\u{201d} to Your Other Machines?")
    );
    assert!(!alert.is_heading_use_markup());
    assert_eq!(
        alert.body().as_str(),
        "It will be copied to your other machines the next time they sync, with its stored \
         password."
    );
    assert!(!alert.is_body_use_markup());
    assert_eq!(alert.default_response().as_deref(), Some("cancel"));
    assert_eq!(alert.close_response().as_str(), "cancel");
    assert_eq!(alert.response_label("cancel").as_str(), "Cancel");
    assert_eq!(alert.response_label("sync").as_str(), "Sync");
    assert_eq!(
        alert.response_appearance("sync"),
        adw::ResponseAppearance::Suggested
    );
    assert!(!row.is_sensitive());
    pump_for(Duration::from_millis(300));
    assert_eq!(s.file_bytes(), before, "nothing written before the answer");

    // Cancel: still kept here, nothing written.
    button_labeled(&alert, "Cancel")
        .expect("the Cancel button")
        .emit_clicked();
    wait_for_the_alert_to_go(&s);
    assert!(!row.is_active() && row.is_sensitive());
    assert_eq!(row.subtitle().as_deref(), Some(KEPT_SUBTITLE));
    pump_for(Duration::from_millis(200));
    assert_eq!(s.file_bytes(), before);

    // Sync: saved.
    row.set_active(true);
    let alert = alert_in(&s.window);
    button_labeled(&alert, "Sync")
        .expect("the Sync button")
        .emit_clicked();
    pump_until("the file to say it syncs", || {
        s.on_disk(&p.id).is_some_and(|stored| !stored.local_only)
    });
    saved();
    pump_until("the row to be ready again", || row.is_sensitive());
    assert!(row.is_active());
    assert_eq!(row.subtitle().as_deref(), Some(SYNCS_SUBTITLE));
    assert!(s.on_disk(&p.id).expect("there").updated_at > p.updated_at);
    s.close();
}

/// A profile pinned at the largest version by another machine cannot sync
/// again (`form::CANNOT_SYNC_AGAIN`): the switch says why and goes back.
pub(super) fn a_profile_that_cannot_sync_again_says_so(dir: &Path) {
    let mut p = Profile::new("Box", "box.example.test", "alice");
    p.local_only = true;
    let s = Setup::open(dir, std::slice::from_ref(&p));
    // Pinned the way a peer's edit would: `upsert` restamps, so write it whole.
    let mut pinned = p.clone();
    pinned.updated_at = i64::MAX;
    s.other_store()
        .replace_all(vec![pinned])
        .expect("pin the version");
    s.ctx.profiles_changed();
    s.select(&p.id);
    let row = s.switch(SYNC_ROW);
    let before = s.file_bytes();

    row.set_active(true);
    button_labeled(&alert_in(&s.window), "Sync")
        .expect("the Sync button")
        .emit_clicked();
    saved();
    pump_until("the row to be ready again", || row.is_sensitive());

    assert!(!row.is_active(), "back to kept");
    assert!(row.has_css_class("error"));
    assert_eq!(
        s.ctx.notice.last_text().as_deref(),
        Some(form::CANNOT_SYNC_AGAIN)
    );
    assert_eq!(s.file_bytes(), before, "nothing written");
    s.close();
}

/// A second flip while the first is still being saved starts nothing (review
/// focus 5): the row is insensitive from the first flip until its save is
/// back, a flip that gets in anyway (code, not a pointer) opens no second
/// question and asks for no second save, and the file ends as the first
/// confirmed it.
pub(super) fn a_second_flip_waits_for_the_first(dir: &Path) {
    let p = Profile::new("Box", "box.example.test", "alice");
    let s = Setup::open(dir, std::slice::from_ref(&p));
    s.select(&p.id);
    let row = s.switch(SYNC_ROW);
    let before = s.file_bytes();
    let held = HeldProfileFile::hold(dir);

    row.grab_focus();
    row.set_active(false);
    let alert = alert_in(&s.window);
    let closed = Rc::new(Cell::new(false));
    alert.connect_closed({
        let closed = Rc::clone(&closed);
        move |_| closed.set(true)
    });
    button_labeled(&alert, "Keep on This Machine Only")
        .expect("the Keep button")
        .emit_clicked();
    pump_until("the question to close", || closed.get());
    wait_for_the_alert_to_go(&s);
    assert_eq!(keyring::pending(), 1, "one save, waiting for the file");
    assert!(!row.is_sensitive(), "the row waits for its save");
    pump_until("the row's spinner", || spinner_shown(&row));
    assert!(!row.is_active(), "it shows what was asked");

    // Flipped back by code while the save waits: no question, no second save.
    row.set_active(true);
    pump_for(Duration::from_millis(300));
    assert!(
        all_widgets::<adw::AlertDialog>(&s.window).is_empty(),
        "no second question"
    );
    assert_eq!(keyring::pending(), 1, "and no second save");
    assert!(!row.is_active(), "still showing what was confirmed");
    assert!(!row.is_sensitive());
    // A refresh meanwhile (a sync elsewhere, say) puts nothing back.
    s.ctx.profiles_changed();
    pump_for(Duration::from_millis(100));
    assert!(!row.is_active());
    assert_eq!(s.file_bytes(), before, "nothing written while it is held");

    // The person moves on while the save waits: the keyboard stays where they
    // put it, it is not pulled back to the row when the save is.
    let host = s.entry("Host");
    host.grab_focus();

    held.release();
    saved();
    pump_until("the row to be ready again", || row.is_sensitive());
    assert!(s.on_disk(&p.id).expect("there").local_only);
    assert!(!row.is_active());
    assert_eq!(row.subtitle().as_deref(), Some(KEPT_SUBTITLE));
    assert!(!spinner_shown(&row));
    assert!(all_widgets::<adw::AlertDialog>(&s.window).is_empty());
    assert!(
        has_focus_in(&s.window, &host) && !has_focus_in(&s.window, &row),
        "the keyboard stays where the person put it"
    );
    s.close();
}

/// A question that is open when its profile goes (a sync elsewhere deleted it)
/// goes with it: its Keep or Sync button would have nothing to act on, and a
/// question naming a profile that is gone only misleads. Nothing is written.
pub(super) fn a_question_closes_with_its_profile(dir: &Path) {
    let p = Profile::new("Box", "box.example.test", "alice");
    let s = Setup::open(dir, std::slice::from_ref(&p));
    s.ctx.watch_profile_file();
    s.select(&p.id);
    let row = s.switch(SYNC_ROW);

    row.set_active(false);
    let alert = alert_in(&s.window);
    assert_eq!(
        alert.heading().as_deref(),
        Some("Keep \u{201c}Box\u{201d} on This Machine Only?")
    );

    // Another process (a sync) deletes the profile; nobody calls
    // `profiles_changed`, the file watch does.
    s.other_store().delete(&p.id).expect("delete");
    pump_until("the window to follow the file", || s.shows_empty_state());
    wait_for_the_alert_to_go(&s);
    let after_delete = s.file_bytes();

    // Nothing is written back, now or later.
    pump_for(Duration::from_millis(300));
    assert_eq!(keyring::pending(), 0, "no save was asked for");
    assert_eq!(s.file_bytes(), after_delete, "nothing written");
    assert!(s.on_disk(&p.id).is_none(), "the profile stays deleted");
    s.close();
}

/// What the mark of a profile kept on this machine says, in its tooltip and to
/// a screen reader.
const KEPT_MARK_TEXT: &str = "On this machine only";

/// The marks of kept profiles that `row` holds: the images that say so by their
/// icon or by their text, whatever else they are.
fn kept_marks(row: &impl IsA<gtk::Widget>) -> Vec<gtk::Image> {
    all_widgets::<gtk::Image>(row)
        .into_iter()
        .filter(|image| {
            image.icon_name().as_deref() == Some("computer-symbolic")
                || image.tooltip_text().as_deref() == Some(KEPT_MARK_TEXT)
        })
        .collect()
}

/// `row` (of the list called `what`) holds one mark, a plain image that says
/// "On this machine only", is not in the keyboard's way, and is shown.
fn assert_marked(what: &str, row: &adw::ActionRow) -> gtk::Image {
    let marks = kept_marks(row);
    assert_eq!(marks.len(), 1, "{what}: one mark in the row");
    let mark = marks.into_iter().next().expect("the mark");
    assert_eq!(mark.icon_name().as_deref(), Some("computer-symbolic"));
    assert_eq!(mark.tooltip_text().as_deref(), Some(KEPT_MARK_TEXT));
    assert!(mark.has_css_class("dim-label"), "{what}: dimmed");
    assert_eq!(mark.accessible_role(), gtk::AccessibleRole::Img);
    assert!(!mark.is_focusable(), "{what}: the mark takes no keyboard");
    pump_until("the mark to show", || mark.is_mapped());
    mark
}

/// A kept profile is marked in both lists, and no other is (issue #10): the
/// sidebar of the Connection Profiles window and the new-tab list each hold one
/// image, "computer-symbolic" with the tooltip "On this machine only", in the
/// row of a profile kept on this machine, and none in the row of one that
/// syncs. In the new-tab list it sits before the "›" button. A profile file
/// changed elsewhere (a sync, another window) changes the marks in both lists
/// at once, in both directions.
pub(super) fn the_lists_mark_profiles_kept_here(dir: &Path) {
    let mut kept = Profile::new("Kept", "kept.example.test", "alice");
    kept.local_only = true;
    let other = Profile::new("Other", "other.example.test", "bob");
    let s = Setup::open(dir, &[kept.clone(), other.clone()]);
    s.ctx.watch_profile_file();
    let main = window::build(&s._app, &s.ctx);
    main.window.present();
    pump_for(Duration::from_millis(300));
    let page = new_tab::open(&main.view, &s.ctx);
    pump_for(Duration::from_millis(300));
    let list = list_in(&page).expect("the new-tab list");
    let sidebar_row = |p: &Profile| row_for(&s.sidebar(), &p.id);
    let list_row = |p: &Profile| row_for(&list, &p.id);

    assert_marked("sidebar", &sidebar_row(&kept));
    let mark = assert_marked("new-tab list", &list_row(&kept));
    let more = all_widgets::<gtk::MenuButton>(&list_row(&kept))
        .into_iter()
        .find(|b| b.tooltip_text().as_deref() == Some("Connect Options"))
        .expect("the › button");
    assert_eq!(
        mark.next_sibling().as_ref(),
        Some(more.upcast_ref::<gtk::Widget>()),
        "the mark comes before the › button"
    );
    assert!(kept_marks(&sidebar_row(&other)).is_empty(), "syncs: none");
    assert!(kept_marks(&list_row(&other)).is_empty(), "syncs: none");

    // Another process keeps the other one here: its rows gain the mark.
    let mut changed = other.clone();
    changed.local_only = true;
    s.other_store().upsert(changed).expect("save");
    pump_until("both lists to mark it", || {
        !kept_marks(&sidebar_row(&other)).is_empty() && !kept_marks(&list_row(&other)).is_empty()
    });
    assert_marked("sidebar", &sidebar_row(&other));
    assert_marked("new-tab list", &list_row(&other));
    assert_marked("sidebar", &sidebar_row(&kept));
    assert_marked("new-tab list", &list_row(&kept));

    // And one that syncs again loses it.
    let mut changed = kept.clone();
    changed.local_only = false;
    s.other_store().upsert(changed).expect("save");
    pump_until("both lists to unmark it", || {
        kept_marks(&sidebar_row(&kept)).is_empty() && kept_marks(&list_row(&kept)).is_empty()
    });
    assert_marked("sidebar", &sidebar_row(&other));
    assert_marked("new-tab list", &list_row(&other));

    main.window.destroy();
    s.close();
}

/// The New Profile dialog's "Sync to other machines" switch (issue #10): on
/// unless turned off, with the same subtitles as the profile page's. A profile
/// created with it off is kept on this machine only from the start, with no
/// question (nothing of it exists on another machine yet), and shows the mark
/// and the profile page's switch off; one created with it on syncs.
pub(super) fn a_new_profile_can_be_kept_here(dir: &Path) {
    let s = Setup::open(dir, &[web()]);

    // Left on, as it is: the profile syncs.
    let synced = create_in_the_dialog(&s, "Synced", "synced.lan", None, true);
    assert!(!s.on_disk(&synced).expect("saved").local_only);
    assert!(kept_marks(&row_for(&s.sidebar(), &synced)).is_empty());
    assert!(s.switch(SYNC_ROW).is_active());

    // Turned off: kept here, with its password, with no dialog.
    let kept = create_in_the_dialog(&s, "Kept", "kept.lan", Some("kept-password"), false);
    assert!(s.on_disk(&kept).expect("saved").local_only);
    assert_eq!(s.secrets.password(&kept).as_deref(), Some("kept-password"));
    assert_marked("sidebar", &row_for(&s.sidebar(), &kept));
    assert!(kept_marks(&row_for(&s.sidebar(), &synced)).is_empty());
    let selected = s.sidebar().selected_row().expect("a selection");
    assert_eq!(selected.widget_name(), kept.as_str());
    let page_switch = s.switch(SYNC_ROW);
    assert!(!page_switch.is_active(), "the page shows it kept here");
    assert_eq!(page_switch.subtitle().as_deref(), Some(KEPT_SUBTITLE));
    pump_for(Duration::from_millis(300));
    assert!(
        s.window.visible_dialog().is_none(),
        "no question came later"
    );
    s.close();
}

/// Open the New Profile dialog of `s`, fill it in, leave "Sync to other
/// machines" on or turn it off as `sync` says (checking what it shows), press
/// Create and return the new profile's id once the dialog has closed with no
/// question in its place.
fn create_in_the_dialog(
    s: &Setup,
    name: &str,
    host: &str,
    password: Option<&str>,
    sync: bool,
) -> String {
    let add = all_widgets::<gtk::Button>(&s.window)
        .into_iter()
        .find(|b| b.tooltip_text().as_deref() == Some("Add Profile"))
        .expect("the + button");
    add.emit_clicked();
    pump_until("the New Profile dialog", || {
        s.window.visible_dialog().is_some()
    });
    let dialog = s.window.visible_dialog().expect("the New Profile dialog");
    let entry = |title: &str| {
        all_widgets::<adw::EntryRow>(&dialog)
            .into_iter()
            .find(|r| r.title() == title)
            .unwrap_or_else(|| panic!("no {title:?} row"))
    };
    let switches = all_widgets::<adw::SwitchRow>(&dialog);
    assert_eq!(switches.len(), 1, "one switch in the dialog");
    let switch = &switches[0];
    assert_eq!(switch.title(), SYNC_ROW);
    assert!(!switch.uses_markup());
    assert!(switch.is_active(), "on unless the person turns it off");
    assert_eq!(switch.subtitle().as_deref(), Some(SYNCS_SUBTITLE));
    if !sync {
        switch.set_active(false);
        assert_eq!(switch.subtitle().as_deref(), Some(KEPT_SUBTITLE));
    }
    entry("Name").set_text(name);
    entry("Host").set_text(host);
    entry("Username").set_text("admin");
    if let Some(password) = password {
        entry("Password (optional)").set_text(password);
    }
    button_labeled(&dialog, "Create")
        .expect("the Create button")
        .emit_clicked();
    saved();
    pump_until("the dialog to close", || {
        s.window.visible_dialog().is_none()
    });
    assert!(
        all_widgets::<adw::AlertDialog>(&s.window).is_empty(),
        "no question when nothing exists elsewhere yet"
    );
    s.ctx
        .store
        .borrow()
        .live()
        .iter()
        .find(|p| p.name == name)
        .map(|p| p.id.clone())
        .expect("the new profile")
}

// ----- Local terminal (issue #13) -----

/// The subtitle of every "Save transcript" switch of the window, a profile's
/// and Local terminal's.
const TRANSCRIPT_SUBTITLE: &str = "Saves the session as plain text when its tab closes, \
     including anything you type or paste. Keep passwords out of it.";

/// The sidebar's Local terminal entry sits above the profiles, whatever the
/// search, and shows Local terminal's page: its startup sequences and its own
/// "Save transcript" switch, which saves into the settings file and never the
/// profile file. Selecting it, a profile or Sync unselects the others, and the
/// header's button reads Open for Local terminal and Connect for a profile.
/// Opening it (a double click or Enter on the entry, or Open) asks for a local
/// terminal running its default sequence, with a transcript as its switch and
/// "Save transcripts of all sessions" say, as the settings file has them then.
pub(super) fn the_local_terminal_entry_and_its_page(dir: &Path) {
    let p = web();
    let s = Setup::open(dir, std::slice::from_ref(&p));
    let profile_file = s.file_bytes();

    // One row, plain text, with the terminal icon, above the profiles (the
    // Sync entry is below them).
    let (entry, row) = s.local_entry();
    assert!(entry.row_at_index(1).is_none(), "one row");
    // Plain text, and no subtitle (libadwaita's is empty, not none).
    assert_eq!(row.title(), "Local terminal");
    assert!(!row.uses_markup());
    literal_labels("Local terminal entry", &row, "Local terminal");
    assert!(row.subtitle().as_deref().is_none_or(str::is_empty));
    assert!(
        all_widgets::<gtk::Image>(&row)
            .iter()
            .any(|image| image.icon_name().as_deref() == Some("utilities-terminal-symbolic")),
        "the terminal icon"
    );
    let order: Vec<String> = all_widgets::<gtk::ListBox>(&s.window)
        .iter()
        .map(|list| list.widget_name().to_string())
        .filter(|name| {
            [
                profiles::LOCAL_ENTRY_NAME,
                profiles::SIDEBAR_NAME,
                profiles::SYNC_ENTRY_NAME,
            ]
            .contains(&name.as_str())
        })
        .collect();
    assert_eq!(
        order,
        [
            profiles::LOCAL_ENTRY_NAME,
            profiles::SIDEBAR_NAME,
            profiles::SYNC_ENTRY_NAME
        ]
    );
    assert!(row.is_mapped());

    // A search that matches no profile leaves it on show.
    let search: gtk::SearchEntry = find_widget(&s.window).expect("the search entry");
    search.set_text("zzz");
    pump_until("the filter to apply", || {
        s.rows().iter().all(|r| !r.is_child_visible())
    });
    assert!(row.is_child_visible() && row.is_mapped(), "never filtered");
    search.set_text("");
    pump_until("the filter to clear", || {
        s.rows().iter().all(|r| r.is_child_visible())
    });

    // Its page.
    s.select_local();
    assert_eq!(s.title(), "Local terminal");
    let open = button_labeled(&s.window, "Open").expect("the header's Open button");
    assert!(open.is_visible() && open.is_mapped());
    assert!(open.has_css_class("suggested-action"));
    assert!(button_labeled(&s.window, "Connect").is_none());
    let groups = s.page_groups();
    let titles: Vec<String> = groups.iter().map(|g| g.title().to_string()).collect();
    assert_eq!(titles, ["Startup sequences", "Session"]);
    assert_eq!(
        groups[0].description().as_deref(),
        Some(LOCAL_SEQUENCES_NOTE)
    );
    assert_eq!(
        s.group_row_titles(),
        ["None: opening gives a plain shell", "Add sequence"]
    );
    assert!(
        all_widgets::<adw::ComboRow>(&s.window).is_empty(),
        "no Default sequence row with no sequence"
    );
    let switch = s.switch("Save transcript");
    assert!(!switch.is_active());
    assert!(!switch.uses_markup());
    assert_eq!(switch.subtitle().as_deref(), Some(TRANSCRIPT_SUBTITLE));
    assert!(switch.is_ancestor(&groups[1]), "in the Session group");

    // The switch saves into the settings file at once, not the profile file,
    // and every list and window hears of it.
    let heard = s.heard();
    switch.set_active(true);
    assert!(s.local().transcript);
    assert!(switch.is_active());
    assert_eq!(s.file_bytes(), profile_file);
    assert_eq!(heard.get(), 1, "told");

    // A profile, then the entry again: each unselects the other, and the
    // button says what it does.
    s.select(&p.id);
    assert_eq!(s.title(), "web");
    assert!(entry.selected_row().is_none(), "the entry unselected");
    let connect = button_labeled(&s.window, "Connect").expect("the Connect button");
    assert_eq!(connect, open, "the same button");
    assert!(connect.is_visible());
    assert!(button_labeled(&s.window, "Open").is_none());
    s.select_local();
    assert_eq!(s.title(), "Local terminal");
    assert!(
        s.sidebar().selected_row().is_none(),
        "the profile unselected"
    );
    assert!(button_labeled(&s.window, "Open").is_some_and(|b| b.is_visible()));
    assert!(s.switch("Save transcript").is_active(), "as the file says");

    // Sync, then the entry again.
    let sync = all_widgets::<gtk::ListBox>(&s.window)
        .into_iter()
        .find(|list| list.widget_name() == profiles::SYNC_ENTRY_NAME)
        .expect("the Sync entry");
    let sync_row = sync.row_at_index(0).expect("its row");
    sync.emit_by_name::<()>("row-activated", &[&sync_row]);
    pump_for(Duration::from_millis(100));
    assert_eq!(s.title(), "Sync");
    assert!(entry.selected_row().is_none(), "the entry unselected");
    assert!(!open.is_visible(), "no button for the sync page");
    assert_eq!(open.label().as_deref(), Some("Connect"));
    s.select_local();
    assert_eq!(s.title(), "Local terminal");
    assert!(sync.selected_row().is_none(), "Sync unselected");
    assert!(open.is_visible());
    assert_eq!(open.label().as_deref(), Some("Open"));

    // A profile's sequence sub-page open, with text typed in it: selecting the
    // entry closes it, shows Local terminal's page and writes nothing; and the
    // other way round.
    s.select(&p.id);
    let profile_before = s.file_bytes();
    let settings_before = s.settings_bytes();
    let page = s.open_sequence("Add sequence");
    sub_name(&page).set_text("unsaved");
    set_commands(&page, "echo unsaved");
    s.select_local();
    s.wait_for_sub_page_to_close();
    assert_eq!(
        s.content_nav()
            .visible_page()
            .and_then(|page| page.tag())
            .as_deref(),
        Some(profiles::PROFILE_TAG)
    );
    assert_eq!(s.title(), "Local terminal");
    assert_eq!(
        s.page_groups()[0].description().as_deref(),
        Some(LOCAL_SEQUENCES_NOTE),
        "Local terminal's page"
    );
    assert_eq!(s.file_bytes(), profile_before, "nothing saved");
    assert_eq!(s.settings_bytes(), settings_before, "nothing saved");
    let page = s.open_sequence("Add sequence");
    sub_name(&page).set_text("unsaved");
    set_commands(&page, "echo unsaved");
    s.select(&p.id);
    s.wait_for_sub_page_to_close();
    assert_eq!(
        s.content_nav()
            .visible_page()
            .and_then(|page| page.tag())
            .as_deref(),
        Some(profiles::PROFILE_TAG)
    );
    assert_eq!(s.title(), "web");
    assert_eq!(s.file_bytes(), profile_before, "nothing saved");
    assert_eq!(s.settings_bytes(), settings_before, "nothing saved");
    s.select_local();

    // Opening: a double click or Enter on the entry, and Open.
    assert!(s.connected.borrow().is_empty(), "selecting opens nothing");
    entry.emit_by_name::<()>("row-activated", &[&row]);
    assert_eq!(*s.connected.borrow(), [Opened::Local(None, true)]);

    // A default saved since, in another window: read when it opens.
    local_terminal::apply_sequences(
        &s.ctx.settings_path,
        None,
        |current| form::with_sequence(current, None, seq("Hello", &["printf hi"])),
        |default| default,
    )
    .expect("add a sequence");
    local_terminal::set_default(
        &s.ctx.settings_path,
        Some((0, seq("Hello", &["printf hi"]))),
    )
    .expect("choose it");
    entry.emit_by_name::<()>("row-activated", &[&row]);
    open.emit_clicked();
    assert_eq!(
        *s.connected.borrow(),
        [
            Opened::Local(None, true),
            Opened::Local(Some("Hello".to_owned()), true),
            Opened::Local(Some("Hello".to_owned()), true),
        ]
    );

    // Its own switch off: "Save transcripts of all sessions" still saves one.
    local_terminal::set_transcript(&s.ctx.settings_path, false).expect("switch off");
    open.emit_clicked();
    assert_eq!(
        s.connected.borrow().last(),
        Some(&Opened::Local(Some("Hello".to_owned()), false))
    );
    settings::update(&s.ctx.settings_path, |x| x.save_all_transcripts = true)
        .expect("write the settings");
    open.emit_clicked();
    assert_eq!(
        s.connected.borrow().last(),
        Some(&Opened::Local(Some("Hello".to_owned()), true))
    );

    // A profile's Connect still connects the profile.
    s.select(&p.id);
    connect.emit_clicked();
    assert_eq!(
        s.connected.borrow().last(),
        Some(&Opened::Profile(p.id.clone(), None, true))
    );
    assert_eq!(s.file_bytes(), profile_file, "the profile file untouched");
    s.close();
}

/// Local terminal's sequences are edited on the profile's sub-page, which saves
/// them into the settings file (never the profile file), with the same rules:
/// the default follows Move and Delete, a pick saves it, and the file as it is
/// has the last word (a sequence changed meanwhile refuses Save, a list changed
/// meanwhile refuses a pick, and a default past the end shows as "None"). A
/// refresh that brings nothing new to the rows leaves them as they are.
pub(super) fn local_sequences_are_edited_like_a_profile_s(dir: &Path) {
    let s = Setup::open(dir, &[web()]);
    let profile_file = s.file_bytes();
    s.select_local();

    // Add: the sub-page warns about the settings file.
    let page = s.open_sequence("Add sequence");
    assert_eq!(page.title(), "New Startup Sequence");
    let commands = all_widgets::<adw::PreferencesGroup>(&page)
        .into_iter()
        .find(|group| group.title() == "Commands, one per line")
        .expect("the commands group");
    assert_eq!(
        commands.description().as_deref(),
        Some(LOCAL_SECRETS_WARNING)
    );
    sub_name(&page).set_text("Hello");
    set_commands(&page, "printf hi");
    s.click(&page, "Save");
    s.wait_for_sub_page_to_close();
    assert_eq!(s.local().sequences, [seq("Hello", &["printf hi"])]);
    assert_eq!(s.sequence_row_titles(), ["Hello", "Add sequence"]);
    assert_eq!(
        s.sequence_rows()[0].subtitle().as_deref(),
        Some("1 command")
    );
    let combo = s.default_row();
    assert_eq!(
        combo.subtitle().as_deref(),
        Some("Runs every time Local terminal is picked or restored")
    );
    assert!(!combo.uses_markup());
    assert_eq!(items_of(&combo), ["None", "Hello"]);
    assert_eq!(shown_item(&combo), "None");
    assert_eq!(
        s.group_row_titles(),
        ["Hello", "Default sequence", "Add sequence"]
    );
    assert_eq!(s.file_bytes(), profile_file);

    // A pick saves the default, and every list and window hears of it (from
    // the main loop).
    let heard = s.heard();
    s.pick_default("Hello");
    assert_eq!(s.local().startup_sequence, Some(0));
    assert_eq!(shown_item(&combo), "Hello");
    pump_until("the lists to hear of the pick", || heard.get() == 1);

    // Move Up takes the default along.
    let page = s.open_sequence("Add sequence");
    sub_name(&page).set_text("Second");
    set_commands(&page, "uptime");
    s.click(&page, "Save");
    s.wait_for_sub_page_to_close();
    let page = s.open_sequence("Second");
    assert_eq!(page.title(), "Startup Sequence");
    assert_eq!(
        order_description(&page).as_deref(),
        Some("Local terminal's options list sequences in this order.")
    );
    s.click(&page, "Move Up");
    assert_eq!(
        s.local().sequences,
        [seq("Second", &["uptime"]), seq("Hello", &["printf hi"])]
    );
    assert_eq!(s.local().startup_sequence, Some(1), "still on Hello");
    assert!(s.sequence_page().is_some(), "still on the sub-page");
    assert_eq!(s.sequence_row_titles(), ["Second", "Hello", "Add sequence"]);
    assert_eq!(shown_item(&combo), "Hello");
    s.click(&page, "Cancel");
    s.wait_for_sub_page_to_close();

    // Delete Sequence on the default clears it.
    let page = s.open_sequence("Hello");
    s.click(&page, "Delete Sequence");
    s.wait_for_sub_page_to_close();
    assert_eq!(s.local().sequences, [seq("Second", &["uptime"])]);
    assert_eq!(s.local().startup_sequence, None);
    assert_eq!(items_of(&combo), ["None", "Second"]);
    assert_eq!(shown_item(&combo), "None");
    assert_eq!(s.file_bytes(), profile_file);

    // The rows are made again only when what they show changes: a command's
    // text changed elsewhere leaves them (the same rows, in the same order); a
    // name or a number of commands makes them again.
    let before = s.sequence_rows();
    s.local_elsewhere(&[seq("Second", &["df -h"])], None);
    s.ctx.profiles_changed();
    assert_eq!(s.sequence_rows(), before, "the same rows");
    s.local_elsewhere(&[seq("Renamed", &["df -h"])], None);
    s.ctx.profiles_changed();
    let renamed = s.sequence_rows();
    assert_eq!(s.sequence_row_titles(), ["Renamed", "Add sequence"]);
    assert_ne!(renamed[0], before[0], "a new name, a new row");
    s.local_elsewhere(&[seq("Renamed", &["df -h", "uptime"])], None);
    s.ctx.profiles_changed();
    let longer = s.sequence_rows();
    assert_ne!(longer[0], renamed[0], "a new count, a new row");
    assert_eq!(longer[0].subtitle().as_deref(), Some("2 commands"));

    // The sequence changed in the file while its sub-page is open: Save is
    // refused, and the file keeps that change.
    let page = s.open_sequence("Renamed");
    local_terminal::apply_sequences(
        &s.ctx.settings_path,
        None,
        |current| form::with_sequence(current, Some(0), seq("Theirs", &["ls"])),
        |default| default,
    )
    .expect("change it elsewhere");
    set_commands(&page, "whoami");
    s.click(&page, "Save");
    assert_eq!(inline_error(&page).as_deref(), Some(form::SEQUENCE_CHANGED));
    assert_eq!(
        s.ctx.notice.last_text().as_deref(),
        Some(form::SEQUENCE_CHANGED)
    );
    assert!(s.sequence_page().is_some(), "the page stays");
    assert_eq!(s.local().sequences, [seq("Theirs", &["ls"])]);
    s.click(&page, "Cancel");
    s.wait_for_sub_page_to_close();
    assert_eq!(s.sequence_row_titles(), ["Theirs", "Add sequence"]);

    // The list changed in the file while the default row shows it: a pick is
    // refused, nothing is written, and the row shows the file again.
    s.local_elsewhere(&[seq("a", &["echo A"]), seq("b", &["echo B"])], None);
    s.ctx.profiles_changed();
    assert_eq!(items_of(&combo), ["None", "a", "b"]);
    s.local_elsewhere(&[seq("b", &["echo B"]), seq("a", &["echo A"])], None);
    let written = s.settings_bytes();
    s.pick_default("a");
    assert_eq!(
        s.ctx.notice.last_text().as_deref(),
        Some(form::CHOICE_CHANGED)
    );
    assert_eq!(s.settings_bytes(), written, "nothing was written");
    assert!(combo.has_css_class("error"));
    // The row shows the file again from the main loop, not from inside its own
    // signal.
    pump_until("the row to show the file's list", || {
        items_of(&combo) == ["None", "b", "a"]
    });
    assert_eq!(shown_item(&combo), "None", "the file's choice");
    // The same pick on the list as it is now works, and clears the mark.
    s.pick_default("a");
    assert_eq!(s.local().startup_sequence, Some(1));
    assert!(!combo.has_css_class("error"));

    // A default past the end of the list (a hand edit) shows as "None", and
    // showing it writes nothing.
    s.local_elsewhere(&[seq("a", &["echo A"])], Some(7));
    let written = s.settings_bytes();
    s.ctx.profiles_changed();
    assert_eq!(items_of(&combo), ["None", "a"]);
    assert_eq!(shown_item(&combo), "None");
    assert_eq!(s.settings_bytes(), written);
    assert_eq!(s.local().startup_sequence, Some(7), "left as it was");
    assert_eq!(s.file_bytes(), profile_file, "the profile file untouched");
    s.close();
}

/// A settings file that cannot be read: Local terminal's page shows what the
/// defaults say, and every change is refused, the file left as it is.
pub(super) fn a_damaged_settings_file_refuses_local_changes(dir: &Path) {
    let s = Setup::open(dir, &[web()]);
    fs::write(&s.ctx.settings_path, "not json").expect("damage the settings");
    s.select_local();
    assert_eq!(
        s.group_row_titles(),
        ["None: opening gives a plain shell", "Add sequence"]
    );

    // The switch goes back, with the reason.
    let switch = s.switch("Save transcript");
    switch.set_active(true);
    assert!(!switch.is_active(), "back off");
    assert!(
        s.ctx
            .notice
            .last_text()
            .is_some_and(|text| text.starts_with("Could not save the local terminal: ")),
        "{:?}",
        s.ctx.notice.last_text()
    );

    // A new sequence is not saved, and says why on its page.
    let page = s.open_sequence("Add sequence");
    sub_name(&page).set_text("Hello");
    set_commands(&page, "printf hi");
    s.click(&page, "Save");
    let error = inline_error(&page).expect("the reason on the sub-page");
    assert!(
        error.starts_with("Could not save the local terminal: "),
        "{error}"
    );
    assert!(s.sequence_page().is_some(), "the page stays");
    assert_eq!(
        fs::read_to_string(&s.ctx.settings_path).expect("the settings"),
        "not json"
    );
    s.click(&page, "Cancel");
    s.wait_for_sub_page_to_close();
    let settings_path = s.ctx.settings_path.clone();
    s.close();
    assert_eq!(
        fs::read_to_string(settings_path).expect("the settings"),
        "not json",
        "closing the window wrote nothing either"
    );
}
