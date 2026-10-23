//! Terminal tabs: the local shell and SSH connections made from profiles.
//!
//! The tabs are the pages of an `adw::TabView`; [`setup_tab_view`] and
//! [`setup_tab_bar`] wire one up. A page starts as a holder ([`new_holder_page`]),
//! an empty box that later receives a chooser or a session: [`start_local_shell`],
//! [`start_profile`] and [`start_command`] put a terminal in it, and the
//! `open_*` functions are a new holder page followed by one of those.
//!
//! [`open_profile`] turns a profile into an `ssh` command with
//! `moniker_core::ssh::terminal_command` and runs it in a VTE tab. The command
//! is passed to VTE as an argv list and `KEY=VALUE` entries, never as a shell
//! string: the startup-sequence argument holds newlines and a literal `$SHELL`
//! that must reach the remote shell untouched. VTE adds the entries to the
//! inherited environment; it does not replace it.
//!
//! When a connection ends with a clean exit its tab closes. After a failure
//! (non-zero status, or a signal) the tab stays open with a short note, so
//! the `ssh` error message can still be read. Closing the last tab of a window
//! closes the window.
//!
//! # Stored passwords
//!
//! A connection with a stored password gets a fresh one-time token
//! ([`connection_command`]); the askpass helper sends the stored password only
//! for the prompt that creates the token's marker in the runtime directory, so
//! a wrong one is tried once and then `ssh` asks in the terminal
//! (`moniker_core::askpass`). The tab keeps the marker's name ([`OneTimeMarker`],
//! with the page's holder) and removes the marker only once its `ssh` cannot
//! ask again, which is when its child has exited. Closing the tab or its window
//! hangs the child up and keeps the terminal until the exit is seen
//! ([`end_session`]). A child that never exits keeps its marker (an empty file
//! in the runtime directory, gone at logout), which is always safe. A tab
//! moving to another window takes it along and removes nothing.
//!
//! # Copy and paste
//!
//! Ctrl+Shift+C and Ctrl+Shift+V are the main window's `win.copy` and
//! `win.paste` ([`copy_selection`], [`paste_clipboard`]); Ctrl+C stays the
//! interrupt. Both are enabled whenever the selected tab has a terminal and no
//! dialog is open over the window ([`sync_window_actions`]), and `win.copy`
//! does nothing without a selection: a disabled action would hand
//! Ctrl+Shift+C on to the terminal, which by xterm convention may turn it into
//! Ctrl+C and interrupt the program. Every terminal also has a right-click menu
//! ([`terminal_menu`]): Paste is `win.paste`, and Copy is the terminal's own
//! `term.copy`, which follows that terminal's selection, so the menu shows it
//! insensitive without one.
//!
//! # Text size
//!
//! Each tab has its own text size, its VTE font scale (`crate::zoom`), kept with
//! the page's holder so it stays with the tab when the tab moves to another
//! window, and so a tab that still shows the new-tab list has one to give its
//! session. Ctrl+plus, Ctrl+minus and Ctrl+0 are the main window's `win.zoom-in`,
//! `win.zoom-out` and `win.zoom-reset` ([`zoom_selected`]); they and the ☰
//! menu's zoom row act on the selected tab only, and are enabled while it has a
//! terminal and no dialog is open ([`sync_window_actions`]). A new tab starts at
//! the size of the tab that was selected in its window, or, in a window with
//! none, at the size saved for the next launch ([`new_holder_page_at`]). A size
//! change redraws the terminal once; nothing is done per frame.
//!
//! # Titles
//!
//! A tab's title is a [`TabTitle`], kept with the page's holder. A remote tab is
//! named after its profile and never takes a title from the remote shell: that
//! would let a server make its tab look like another host. A local tab follows
//! its own shell's title. Either can be renamed ([`ask_rename`]), and a renamed
//! tab keeps its name. Titles are plain text, so the tooltip, which GTK reads as
//! markup, is set to the escaped title.
//!
//! # Transcripts
//!
//! A session that records gets unlimited scrollback (other tabs keep the
//! Preferences' scrollback, [`set_scrollback`]), and its text (scrollback and
//! screen, as plain text from VTE) is written once per tab, by whichever comes
//! first of: the child exiting, the tab being closed, the window being closed.
//! Local shells record the same way as connections do. Whether a session
//! records is decided before its tab starts: "Save transcripts of all
//! sessions" in the settings turns it on for every new tab, the launch tab
//! included ([`default_save`]); without it, a connection follows its profile's
//! setting and a local shell saves nothing.
//! Either way the person can choose otherwise for one session with the "Save
//! transcript" box in the choices behind "›" in the new-tab list
//! ([`transcript_spec`], [`local_transcript_spec`]). The folder a session saves
//! to is the one in the settings when the session starts, until the person
//! chooses another folder in the Preferences: then every session still open is
//! pointed at the new folder before the old one's transcripts move there
//! ([`save_open_transcripts_to`]).
//! Only text VTE holds is recorded: full-screen programs on the alternate
//! screen (htop, vim) leave nothing in it, and closing a tab while one is
//! showing records that screen instead of the scrollback. The text is never
//! logged. A transcript that cannot be written is reported as a notification in
//! the window of its tab; a saved one is not announced.
//!
//! # Font and scrollback
//!
//! The Preferences' terminal font ([`apply_font`]) applies to every open
//! terminal at once and to every new one; the scrollback of ordinary tabs
//! ([`set_scrollback`]) applies to tabs opened after the change. Both are kept
//! here for [`new_terminal`] and set once per terminal, never per frame;
//! [`use_preferences`] takes them from the settings file at launch.

use std::cell::{Cell, RefCell};
use std::ffi::{c_int, c_long};
use std::io;
use std::os::fd::AsFd;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use adw::prelude::*;
use gtk::{gio, glib};
use gtk4 as gtk;
use libadwaita as adw;
use moniker_core::askpass::{AskpassToken, release_marker};
use moniker_core::profile::Profile;
use moniker_core::secrets::{SecretStore, password_key};
use moniker_core::ssh::{SshCommand, terminal_command};
use moniker_core::transcript::{transcript_path, write_transcript};
use rustix::ioctl;
use rustix::process::{Pid, Signal, getpgrp, getpid, getsid, kill_process, kill_process_group};
use rustix::termios::tcgetpgrp;
use vte4::prelude::*;
use vte4::{Format, PtyFlags, Terminal};

use crate::context::{AppContext, Notice};
use crate::settings::{self, GuiSettings};
use crate::{theme, zoom};

use self::holder::TabHolder;

/// Tab title for a profile whose name has nothing printable in it.
const FALLBACK_TITLE: &str = "Session";

/// The automatic title of a local tab until its shell sets one.
const LOCAL_TITLE: &str = "Local terminal";

/// What to do when the child process of a connection tab exits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitAction {
    /// Clean exit: close the tab.
    Close,
    /// Failed or killed: keep the tab so the output stays readable, and show
    /// `note` in the terminal.
    Keep { note: String },
}

/// Where and under what name a session's transcript is written, and where its
/// outcome is reported.
#[derive(Clone, PartialEq)]
pub struct TranscriptSpec {
    pub host: String,
    pub dir: PathBuf,
    pub notice: Notice,
}

/// The argv (program first) and the `KEY=VALUE` environment entries for VTE.
pub fn spawn_args(cmd: &SshCommand) -> (Vec<String>, Vec<String>) {
    let argv = std::iter::once(cmd.program.clone())
        .chain(cmd.args.iter().cloned())
        .collect();
    let env = cmd.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    (argv, env)
}

/// Decide from a raw wait status (as given by VTE's `child-exited`) whether
/// the tab closes.
pub fn exit_action(status: i32) -> ExitAction {
    let wait = ExitStatus::from_raw(status);
    let note = match (wait.code(), wait.signal()) {
        (Some(0), _) => return ExitAction::Close,
        (Some(code), _) => format!("[session ended: status {code}]"),
        (None, Some(signal)) => format!("[session ended: signal {signal}]"),
        // Stopped or unknown: not a clean exit either.
        (None, None) => format!("[session ended: status {status}]"),
    };
    ExitAction::Keep { note }
}

/// `raw` as a one-line title: control characters (an escape sequence in a title
/// set by a program, a newline in a name) become spaces and the ends are trimmed.
/// `None` if nothing is left.
pub fn clean_title(raw: &str) -> Option<String> {
    let flat: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let title = flat.trim();
    (!title.is_empty()).then(|| title.to_owned())
}

/// The tab label for a profile: its name, made safe for a one-line label.
pub fn tab_title(profile: &Profile) -> String {
    clean_title(&profile.name).unwrap_or_else(|| FALLBACK_TITLE.to_owned())
}

/// What a tab is called, and where that name comes from.
///
/// There is an automatic title, which is the profile's name for a remote tab and
/// the shell's own title for a local one, and the name the person gave the tab,
/// which wins over it once there is one. Everything here is plain text, already
/// cleaned with [`clean_title`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabTitle {
    automatic: String,
    /// Whether [`set_terminal_title`](TabTitle::set_terminal_title) changes the
    /// automatic title: only a local tab listens to its shell.
    follows_shell: bool,
    renamed: Option<String>,
}

impl TabTitle {
    /// The title of a tab connected to a profile titled `profile_title`. The
    /// remote shell cannot change it.
    pub fn remote(profile_title: String) -> TabTitle {
        TabTitle {
            automatic: clean_title(&profile_title).unwrap_or_else(|| FALLBACK_TITLE.to_owned()),
            follows_shell: false,
            renamed: None,
        }
    }

    /// The title of a local tab: "Local terminal" until its shell sets one.
    pub fn local() -> TabTitle {
        TabTitle {
            automatic: LOCAL_TITLE.to_owned(),
            follows_shell: true,
            renamed: None,
        }
    }

    /// The title the shell in the tab asked for (`raw` is as VTE reports it).
    /// Only a local tab takes it, and a shell that clears its title gets the
    /// default back.
    pub fn set_terminal_title(&mut self, raw: &str) {
        if self.follows_shell {
            self.automatic = clean_title(raw).unwrap_or_else(|| LOCAL_TITLE.to_owned());
        }
    }

    /// Name the tab `raw`; a name with nothing in it (after [`clean_title`])
    /// goes back to the automatic title.
    pub fn rename(&mut self, raw: &str) {
        self.renamed = clean_title(raw);
    }

    /// What the tab is called now.
    pub fn shown(&self) -> &str {
        self.renamed.as_deref().unwrap_or(&self.automatic)
    }

    /// Make `next` the automatic title, as when a tab that held a chooser turns
    /// into a session. A name the person already gave the tab is kept.
    fn adopt(&mut self, next: TabTitle) {
        let renamed = self.renamed.take();
        *self = TabTitle { renamed, ..next };
    }
}

/// The widget that is the child of every page made by [`new_holder_page`]: an
/// empty box that fills the page, which later receives a chooser or a terminal.
/// It also keeps the page's [`TabTitle`] and text size, so they move with the page
/// when it is dragged into another window.
mod holder {
    use std::cell::{Cell, RefCell};

    use gtk::glib;
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;
    use gtk4 as gtk;

    use super::{FALLBACK_TITLE, OneTimeMarker, TabTitle, zoom};

    mod imp {
        use super::*;

        pub struct TabHolder {
            pub(super) title: RefCell<TabTitle>,
            /// The session's one-time askpass marker, until its child exits.
            pub(super) marker: RefCell<Option<OneTimeMarker>>,
            /// The session's child has exited (or never started).
            pub(super) child_gone: Cell<bool>,
            /// The tab's text size: the font scale its terminal has, or will
            /// have once a session replaces the chooser.
            pub(super) scale: Cell<f64>,
        }

        impl Default for TabHolder {
            fn default() -> Self {
                TabHolder {
                    title: RefCell::new(TabTitle::remote(FALLBACK_TITLE.to_owned())),
                    marker: RefCell::new(None),
                    child_gone: Cell::new(false),
                    scale: Cell::new(zoom::DEFAULT),
                }
            }
        }

        #[glib::object_subclass]
        impl ObjectSubclass for TabHolder {
            const NAME: &'static str = "MonikerTabHolder";
            type Type = super::TabHolder;
            type ParentType = gtk::Box;
        }

        impl ObjectImpl for TabHolder {}
        impl WidgetImpl for TabHolder {}
        impl BoxImpl for TabHolder {}
    }

    glib::wrapper! {
        pub struct TabHolder(ObjectSubclass<imp::TabHolder>)
            @extends gtk::Box, gtk::Widget,
            @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
    }

    impl TabHolder {
        /// An empty holder whose tab is titled `title`, at text size `scale`.
        pub(super) fn new(title: TabTitle, scale: f64) -> TabHolder {
            let holder: TabHolder = glib::Object::new();
            holder.set_orientation(gtk::Orientation::Vertical);
            holder.set_hexpand(true);
            holder.set_vexpand(true);
            holder.imp().title.replace(title);
            holder.imp().scale.set(zoom::clamp(scale));
            holder
        }

        /// The tab's title, to read or change.
        pub(super) fn title(&self) -> &RefCell<TabTitle> {
            &self.imp().title
        }

        /// The session's one-time askpass marker, to take out and release.
        pub(super) fn marker(&self) -> &RefCell<Option<OneTimeMarker>> {
            &self.imp().marker
        }

        /// Whether the session's child has exited (or never started).
        pub(super) fn child_gone(&self) -> &Cell<bool> {
            &self.imp().child_gone
        }

        /// The tab's text size.
        pub(super) fn scale(&self) -> &Cell<f64> {
            &self.imp().scale
        }
    }
}

/// The holder inside `page`, if the page was made by [`new_holder_page`].
fn holder_of(page: &adw::TabPage) -> Option<TabHolder> {
    page.child().downcast().ok()
}

/// The tab view that shows `page`, unless the page is not in one right now (it
/// was closed, or it is being dragged between windows).
fn view_of(page: &adw::TabPage) -> Option<adw::TabView> {
    page.child()
        .ancestor(adw::TabView::static_type())
        .and_downcast()
}

/// Show `title` as the title of `page`, with its tooltip. The tooltip is markup
/// for GTK, so the title is escaped: a name with `<b>` in it stays literal.
fn show_title(page: &adw::TabPage, title: &str) {
    page.set_title(title);
    page.set_tooltip(&glib::markup_escape_text(title));
}

/// Apply the page's [`TabTitle`] to the tab.
fn refresh_title(page: &adw::TabPage, holder: &TabHolder) {
    show_title(page, holder.title().borrow().shown());
}

/// Rename the tab of `page` to `name`: the tab shows it, with its tooltip, and
/// keeps it. A name with nothing in it restores the automatic title. Does nothing
/// for a page that [`new_holder_page`] did not make.
pub fn rename_page(page: &adw::TabPage, name: &str) {
    if let Some(holder) = holder_of(page) {
        holder.title().borrow_mut().rename(name);
        refresh_title(page, &holder);
    }
}

/// Ask the person for a new name for the tab of `page` ("Rename Tab"). The field
/// starts out as the title shown now; Rename applies it with [`rename_page`]. A
/// name left as it was changes nothing: renaming is what freezes an automatic
/// title, and that should not happen by pressing Rename without typing.
pub fn ask_rename(page: &adw::TabPage) {
    let Some(holder) = holder_of(page) else {
        return;
    };
    let current = holder.title().borrow().shown().to_owned();
    let entry = gtk::Entry::builder()
        .text(current.as_str())
        .activates_default(true)
        .build();
    let dialog = adw::AlertDialog::builder()
        .heading("Rename Tab")
        .extra_child(&entry)
        .focus_widget(&entry)
        .default_response("rename")
        .close_response("cancel")
        .build();
    dialog.add_responses(&[("cancel", "Cancel"), ("rename", "Rename")]);
    dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
    dialog.choose(
        Some(&page.child()),
        gio::Cancellable::NONE,
        glib::clone!(
            #[weak]
            page,
            #[weak]
            entry,
            move |response| {
                let name = entry.text();
                if response == "rename" && name != current {
                    rename_page(&page, &name);
                }
            }
        ),
    );
}

/// How long [`after_the_menu_lets_go`] waits for the focus to leave the menu
/// before it runs anyway.
const MENU_FOCUS_FALLBACK: std::time::Duration = std::time::Duration::from_millis(300);

/// Run `open` from an idle callback once the keyboard focus of `window` has
/// left the menu it is in. A closing menu hands the focus back to where it was
/// (the terminal) from an event of its own, a few milliseconds after it closes,
/// and an idle callback alone can run before that event: a dialog opened then
/// loses the focus to one of its buttons when it comes, as the terminal behind
/// the dialog cannot take it (seen on slow machines, and now and then on fast
/// ones). With the focus not in a menu (elsewhere or nowhere), or no window,
/// `open` waits for the idle callback only. While it waits, the focus going
/// nowhere is not yet the hand-back; if the focus never lands outside the
/// menu, `open` runs after [`MENU_FOCUS_FALLBACK`].
pub(crate) fn after_the_menu_lets_go(window: Option<&gtk::Window>, open: impl FnOnce() + 'static) {
    let Some(window) = window.filter(|window| focus_in_a_menu(window) == Some(true)) else {
        glib::idle_add_local_once(open);
        return;
    };
    let wait = Rc::new(MenuWait {
        open: RefCell::new(Some(open)),
        handler: Cell::new(None),
        fallback: Cell::new(None),
    });
    let handler = window.connect_focus_widget_notify(glib::clone!(
        #[strong]
        wait,
        move |window| {
            if focus_in_a_menu(window) == Some(false) {
                wait.finish(window);
            }
        }
    ));
    wait.handler.set(Some(handler));
    let fallback = glib::timeout_add_local_once(
        MENU_FOCUS_FALLBACK,
        glib::clone!(
            #[strong]
            wait,
            #[weak]
            window,
            move || {
                // This source is the one running, and it goes by itself.
                wait.fallback.take();
                wait.finish(&window);
            }
        ),
    );
    wait.fallback.set(Some(fallback));
}

/// Whether the keyboard focus of `window` is in a menu (a popover): `None` when
/// nothing has the focus.
fn focus_in_a_menu(window: &gtk::Window) -> Option<bool> {
    gtk::prelude::RootExt::focus(window)
        .map(|focus| focus.ancestor(gtk::Popover::static_type()).is_some())
}

/// What [`after_the_menu_lets_go`] keeps while it waits.
struct MenuWait<F> {
    open: RefCell<Option<F>>,
    handler: Cell<Option<glib::SignalHandlerId>>,
    fallback: Cell<Option<glib::SourceId>>,
}

impl<F: FnOnce() + 'static> MenuWait<F> {
    /// Stop waiting, and run `open` from an idle callback, once.
    fn finish(&self, window: &gtk::Window) {
        if let Some(handler) = self.handler.take() {
            window.disconnect(handler);
        }
        if let Some(fallback) = self.fallback.take() {
            fallback.remove();
        }
        if let Some(open) = self.open.borrow_mut().take() {
            glib::idle_add_local_once(open);
        }
    }
}

/// The tab menu: what the person gets by right clicking a tab.
fn tab_menu() -> gio::Menu {
    let menu = gio::Menu::new();
    menu.append(Some("Rename\u{2026}"), Some("tab.rename"));
    menu
}

/// The tab view's own shortcuts: none. Its shortcut controller runs before the
/// focused terminal sees a key, so each one would take that key from the
/// programs in the terminal: Ctrl+Tab, Alt+1…9 and Alt+0 (irssi, weechat, mc),
/// Ctrl+Home/End and their Shift variants (editors), and so on. The terminal
/// keeps every key except the accelerators in `main.rs`'s `SHORTCUTS`, which
/// the Keyboard Shortcuts dialog lists: Ctrl+Shift+T/W/N (new tab, close tab,
/// new window), Ctrl+Page Up/Down (switching tabs, `win.previous-tab` and
/// `win.next-tab`), Ctrl+Shift+C/V (copy and paste) and Ctrl+plus, Ctrl+minus
/// and Ctrl+0 (the text size).
const TAB_VIEW_SHORTCUTS: adw::TabViewShortcuts = adw::TabViewShortcuts::NONE;

/// Set up `view` as the tabs of a window: the tab menu, the tab view's own
/// shortcuts ([`TAB_VIEW_SHORTCUTS`]), what closing a tab does, and closing the
/// window with its last tab.
///
/// * Closing a page writes its transcript (once), closes it, and ends its
///   session ([`end_session`]): the child is hung up, and the session's
///   one-time askpass marker is removed once the child has exited.
/// * The menu's "Rename…" is the action `tab.rename`, on the page the menu was
///   opened for.
/// * When the last page goes the window closes. A page leaving for another
///   window (`page-detached`) writes nothing and keeps its askpass marker: its
///   session carries on over there.
pub fn setup_tab_view(view: &adw::TabView) {
    view.set_menu_model(Some(&tab_menu()));
    view.set_shortcuts(TAB_VIEW_SHORTCUTS);

    view.connect_close_page(|view, page| {
        if let Some(terminal) = terminal_of(page) {
            write_transcript_of(&terminal);
        }
        view.close_page_finish(page, true);
        if let Some(holder) = holder_of(page) {
            end_session(&holder);
        }
        glib::Propagation::Stop
    });

    view.connect_n_pages_notify(|view| {
        if view.n_pages() == 0
            && let Some(window) = view.root().and_downcast::<gtk::Window>()
        {
            window.close();
        }
    });

    // The action. The menu says which page it was opened for when it opens; it
    // also says so (with no page) when it closes, which may be before the action
    // runs, so that is ignored and the weak reference keeps a closed page away.
    let menu_page = Rc::new(glib::WeakRef::<adw::TabPage>::new());
    view.connect_setup_menu(glib::clone!(
        #[strong]
        menu_page,
        move |_, page| {
            if let Some(page) = page {
                menu_page.set(Some(page));
            }
        }
    ));
    // The menu closes only after its action has run, and hands the keyboard
    // focus back a moment later, which would take it from the dialog's name
    // field. So the dialog opens once the focus has left the menu.
    let rename = gio::SimpleAction::new("rename", None);
    rename.connect_activate(move |_, _| {
        if let Some(page) = menu_page.upgrade() {
            let window = page.child().root().and_downcast::<gtk::Window>();
            after_the_menu_lets_go(window.as_ref(), move || ask_rename(&page));
        }
    });
    let actions = gio::SimpleActionGroup::new();
    actions.add_action(&rename);
    // The menu's popover hangs off the tab bar, not off the view, so the actions
    // go on the window around both.
    let install = move |view: &adw::TabView| {
        if let Some(window) = view.root() {
            window
                .upcast_ref::<gtk::Widget>()
                .insert_action_group("tab", Some(&actions));
        }
    };
    install(view);
    view.connect_root_notify(install);
}

/// Let a double click on a tab's title in `bar` rename the tab.
///
/// The first click of the double click has already selected the tab, so it is the
/// selected page that is renamed. Clicks on a button in a tab (close) or beside
/// the tabs do nothing, so closing tabs in quick succession never asks anything.
pub fn setup_tab_bar(bar: &adw::TabBar) {
    let double_click = gtk::GestureClick::builder()
        .button(gtk::gdk::BUTTON_PRIMARY)
        .propagation_phase(gtk::PropagationPhase::Capture)
        .build();
    double_click.connect_pressed(glib::clone!(
        #[weak]
        bar,
        move |gesture, n_press, x, y| {
            if n_press != 2 || !on_tab_title(&bar, x, y) {
                return;
            }
            // The tab bar sits in the window's title-bar area (a window handle),
            // where a double click also maximizes the window. Claiming the click
            // keeps it for the rename alone.
            gesture.set_state(gtk::EventSequenceState::Claimed);
            if let Some(page) = bar.view().and_then(|view| view.selected_page()) {
                ask_rename(&page);
            }
        }
    ));
    bar.add_controller(double_click);
}

/// Whether the point (`x`, `y`) of `bar` is on a tab and not on a button in it.
fn on_tab_title(bar: &adw::TabBar, x: f64, y: f64) -> bool {
    let mut widget = bar.pick(x, y, gtk::PickFlags::DEFAULT);
    while let Some(current) = widget {
        if current.is::<gtk::Button>() {
            return false;
        }
        // The tab widget is private to libadwaita; its type name is all there is.
        if current.type_().name() == "AdwTab" {
            return true;
        }
        widget = current.parent();
    }
    false
}

/// The transcript settings for a connection to `profile`, if this session
/// records.
///
/// `record` is the choice made at connect time. The connect choices offer it
/// with the profile's own setting (`profile.transcript`) as the default, so it
/// overrides that setting both ways: a profile that records can be connected
/// without, and one that does not can be connected with.
pub fn transcript_spec(
    profile: &Profile,
    record: bool,
    dir: &Path,
    notice: &Notice,
) -> Option<TranscriptSpec> {
    record.then(|| TranscriptSpec {
        host: profile.host.clone(),
        dir: dir.to_path_buf(),
        notice: notice.clone(),
    })
}

/// Whether a session's "Save transcript" choice starts out checked (and what a
/// plain click or Enter does): "Save transcripts of all sessions" (`save_all`)
/// wins over everything, else the profile's own setting. A local terminal has no
/// setting of its own, so it passes `false`. The person can still uncheck one
/// session in the choices behind "›".
pub fn default_save(profile_setting: bool, save_all: bool) -> bool {
    save_all || profile_setting
}

/// This machine's name for itself (the node name `uname` reports), which names
/// the transcripts of local terminals the way a host names those of a
/// connection; `"localhost"` if it has none.
pub fn local_host_name() -> String {
    node_name_or_localhost(&rustix::system::uname().nodename().to_string_lossy())
}

/// `name`, or `"localhost"` if it is empty.
fn node_name_or_localhost(name: &str) -> String {
    if name.is_empty() {
        "localhost".to_owned()
    } else {
        name.to_owned()
    }
}

/// The transcript settings for a local terminal, if it is to be saved (`save`,
/// see [`default_save`]): named after this machine ([`local_host_name`]) and
/// saved to the transcripts folder of the settings as it is now (a folder
/// chosen later in the Preferences takes the session along:
/// [`save_open_transcripts_to`]).
pub fn local_transcript_spec(ctx: &AppContext, save: bool) -> Option<TranscriptSpec> {
    save.then(|| TranscriptSpec {
        host: local_host_name(),
        dir: ctx.transcripts_dir(),
        notice: ctx.notice.clone(),
    })
}

/// What to tell the person after trying to write a transcript: nothing if it
/// was saved, the reason if it was not.
pub fn transcript_notice(result: &io::Result<PathBuf>) -> Option<String> {
    match result {
        Ok(_) => None,
        Err(err) => Some(format!("Could not save the transcript: {err}")),
    }
}

/// What to feed a terminal to show `note` on a line of its own: after the
/// output so far (starting a new line first unless the cursor is already at
/// the start of one), and followed by a line break.
pub fn note_bytes(note: &str, at_line_start: bool) -> Vec<u8> {
    let lead = if at_line_start { "" } else { "\r\n" };
    format!("{lead}{note}\r\n").into_bytes()
}

/// Show `note` in `terminal`, below what it already shows.
fn show_note(terminal: &Terminal, note: &str) {
    let (column, _row) = terminal.cursor_position();
    terminal.feed(&note_bytes(note, column == 0));
}

/// The extracted terminal text as a file body: the blank rows below the last
/// line of output are dropped and a non-empty text ends with one newline.
pub fn transcript_body(raw: &str) -> String {
    let text = raw.trim_end();
    if text.is_empty() {
        String::new()
    } else {
        format!("{text}\n")
    }
}

/// Seconds since the Unix epoch (0 if the clock is before it).
fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

/// Everything VTE holds for `terminal` as plain text: the scrollback and the
/// screen, without escape sequences.
///
/// `get_text_format` would return only the visible rows, so this asks for the
/// range between the bounds of the vertical adjustment instead: `lower` is
/// the first row of the scrollback and `upper` is one past the last row of the
/// screen. A column of -1 means the end of the row.
pub(crate) fn terminal_text(terminal: &Terminal) -> String {
    let Some(adjustment) = terminal.vadjustment() else {
        return String::new();
    };
    let first = adjustment.lower().floor() as c_long;
    let last = adjustment.upper().ceil() as c_long - 1;
    let (text, _length) = terminal.text_range_format(Format::Text, first, 0, last, -1);
    text.map(String::from).unwrap_or_default()
}

/// One transcript-recording session: written at most once, by whichever of the
/// child exiting, the tab closing or the window closing happens first.
struct Session {
    terminal: glib::WeakRef<Terminal>,
    /// Its folder can change while the session runs
    /// ([`save_open_transcripts_to`]).
    spec: RefCell<TranscriptSpec>,
    /// Names the file, so a session keeps the time it started.
    started_secs: i64,
    written: Cell<bool>,
}

thread_local! {
    /// Sessions that have not written their transcript yet, so closing a tab or
    /// the window can find them from its pages. GTK runs on one thread.
    static SESSIONS: RefCell<Vec<Rc<Session>>> = const { RefCell::new(Vec::new()) };
}

impl Session {
    /// Write the transcript unless this session already did, and report a
    /// failure as a notification in the window of its tab. A failure is shown,
    /// never fatal; a saved transcript is not announced.
    fn write_transcript_once(&self) {
        if self.written.replace(true) {
            return;
        }
        SESSIONS.with_borrow_mut(|open| open.retain(|s| !std::ptr::eq(Rc::as_ptr(s), self)));
        let Some(terminal) = self.terminal.upgrade() else {
            return;
        };
        let text = transcript_body(&terminal_text(&terminal));
        // Copied out: showing a notice runs no code of ours, but holding the
        // borrow across it would be an invitation.
        let spec = self.spec.borrow().clone();
        let path = transcript_path(&spec.dir, &spec.host, self.started_secs);
        let result = write_transcript(&path, &text);
        if let Some(message) = transcript_notice(&result) {
            spec.notice.show_near(&terminal, &message);
        }
    }
}

/// Make `terminal`'s session a recording one if it has a `transcript` spec: the
/// terminal keeps unlimited scrollback and the returned [`Session`] writes its
/// text once (module docs). `None` leaves the terminal as it is.
fn start_recording(terminal: &Terminal, transcript: Option<TranscriptSpec>) -> Option<Rc<Session>> {
    transcript.map(|spec| {
        terminal.set_scrollback_lines(-1);
        let session = Rc::new(Session {
            terminal: terminal.downgrade(),
            spec: RefCell::new(spec),
            started_secs: now_unix_secs(),
            written: Cell::new(false),
        });
        register(&session);
        session
    })
}

/// Track `session` until it has written its transcript.
fn register(session: &Rc<Session>) {
    SESSIONS.with_borrow_mut(|open| {
        // Forget sessions whose tab is already gone.
        open.retain(|s| s.terminal.upgrade().is_some());
        open.push(Rc::clone(session));
    });
}

/// Have every session that will still save a transcript save it into `dir`, the
/// transcripts folder the person has just chosen in the Preferences, whatever
/// folder it started with.
///
/// The Preferences call this before the old folder's transcripts are moved
/// into `dir`, and that order is the point: a session that ended during the
/// move and wrote into the old folder could have its file left behind (the
/// move had already listed the folder) or copied while it was still being
/// written. Written into `dir` instead, it is where the person asked for
/// transcripts to be, like everything the move carries over, and no writer
/// touches the old folder while files leave it. A name already taken in `dir`
/// gets a number, as always, so nothing is overwritten either way.
pub(crate) fn save_open_transcripts_to(dir: &Path) {
    SESSIONS.with_borrow(|open| {
        for session in open {
            session.spec.borrow_mut().dir = dir.to_path_buf();
        }
    });
}

/// Write the transcript of the session in `terminal`, if it records and has not
/// written it yet.
fn write_transcript_of(terminal: &Terminal) {
    // Find it first: writing a transcript borrows the registry again.
    let session = SESSIONS.with_borrow(|open| {
        open.iter()
            .find(|s| s.terminal.upgrade().as_ref() == Some(terminal))
            .cloned()
    });
    if let Some(session) = session {
        session.write_transcript_once();
    }
}

/// The pages of `view`, in order.
fn pages_of(view: &adw::TabView) -> Vec<adw::TabPage> {
    let pages = view.pages();
    (0..pages.n_items())
        .filter_map(|i| pages.item(i).and_downcast::<adw::TabPage>())
        .collect()
}

/// Write the transcript of every recording session shown in `view`.
/// Sessions that already wrote theirs are skipped.
pub fn write_transcripts(view: &adw::TabView) {
    let holders: Vec<gtk::Widget> = pages_of(view).iter().map(adw::TabPage::child).collect();
    // Collect first: writing a transcript borrows the registry again.
    let open: Vec<Rc<Session>> = SESSIONS.with_borrow(|open| {
        open.iter()
            .filter(|s| {
                s.terminal
                    .upgrade()
                    .and_then(|t| t.parent())
                    .is_some_and(|holder| holders.contains(&holder))
            })
            .cloned()
            .collect()
    });
    for session in open {
        session.write_transcript_once();
    }
}

/// When `window` is asked to close: write the open sessions' transcripts, then
/// end every session shown in `view` ([`end_session`]: the child is hung up,
/// and its one-time askpass marker is removed once it has exited).
///
/// Ending them here matters: a closed window's tabs are not reliably freed, and
/// their terminals would keep `ssh` running unseen while the app runs on in
/// other windows. A close-request handler that can cancel the closing must be
/// connected before this one, which ends the sessions unconditionally.
pub fn end_sessions_on_close(window: &impl IsA<gtk::Window>, view: &adw::TabView) {
    window.connect_close_request(glib::clone!(
        #[weak]
        view,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_| {
            write_transcripts(&view);
            for page in pages_of(&view) {
                if let Some(holder) = holder_of(&page) {
                    end_session(&holder);
                }
            }
            glib::Propagation::Proceed
        }
    ));
}

thread_local! {
    /// The terminal font of the Preferences; `None` is VTE's own default, the
    /// system monospace font. GTK runs on one thread.
    static FONT: RefCell<Option<gtk::pango::FontDescription>> = const { RefCell::new(None) };
    /// The scrollback of an ordinary tab, in lines (the Preferences). VTE's own
    /// default is 512, which is little for command output; a tab that saves a
    /// transcript keeps everything instead ([`start_recording`]).
    static SCROLLBACK: Cell<c_long> = const { Cell::new(settings::SCROLLBACK_DEFAULT as c_long) };
}

/// Take the terminal font and the scrollback from `settings` (the settings file
/// read at launch): the open terminals get the font, new tabs both.
pub(crate) fn use_preferences(settings: &GuiSettings) {
    apply_font(font_description(settings.font.as_deref()).as_ref());
    set_scrollback(settings.scrollback_lines);
}

/// The font a font setting (a Pango font description string) stands for; `None`
/// for no setting, the system monospace font. A string Pango cannot make sense
/// of still gives a description, whose family the terminal then falls back
/// from.
pub(crate) fn font_description(font: Option<&str>) -> Option<gtk::pango::FontDescription> {
    font.map(gtk::pango::FontDescription::from_string)
}

/// Use `font` in every terminal: at once in every open one, in every window,
/// and in every terminal made from now on ([`new_terminal`]). `None` restores
/// the system monospace font. Each tab's own text size (its font scale) still
/// applies on top. Each terminal is changed once, here; nothing is done per
/// frame.
pub(crate) fn apply_font(font: Option<&gtk::pango::FontDescription>) {
    FONT.replace(font.cloned());
    for terminal in &theme::live_terminals() {
        terminal.set_font(font);
    }
}

/// Give ordinary tabs opened from now on `lines` lines of scrollback (brought
/// into the settings' range). Open tabs keep what they have, and tabs that save
/// a transcript keep unlimited scrollback whatever this says.
pub(crate) fn set_scrollback(lines: i64) {
    let lines = lines.clamp(settings::SCROLLBACK_MIN, settings::SCROLLBACK_MAX);
    SCROLLBACK.set(c_long::try_from(lines).unwrap_or(c_long::MAX));
}

/// A terminal that fills its tab, in the colors of the current light or dark
/// style and the font of the Preferences, with the Preferences' scrollback (a
/// tab that saves a transcript then gets unlimited scrollback instead) and the
/// Copy and Paste menu ([`terminal_menu`]).
fn new_terminal() -> Terminal {
    let terminal = Terminal::new();
    terminal.set_hexpand(true);
    terminal.set_vexpand(true);
    terminal.set_scrollback_lines(SCROLLBACK.get());
    FONT.with_borrow(|font| {
        if let Some(font) = font {
            terminal.set_font(Some(font));
        }
    });
    add_menu_copy_action(&terminal);
    terminal.set_context_menu_model(Some(&terminal_menu()));
    theme::follow_style(&terminal);
    terminal
}

/// Names of the main window's tab actions (`win.new-tab`, `win.close-tab`,
/// `win.next-tab`, `win.previous-tab`: Ctrl+Shift+T, Ctrl+Shift+W, Ctrl+Page
/// Down and Ctrl+Page Up, and the ＋ button). `window.rs` defines them; the
/// accelerators are in `main.rs`'s `SHORTCUTS`.
pub(crate) const NEW_TAB_ACTION: &str = "new-tab";
pub(crate) const CLOSE_TAB_ACTION: &str = "close-tab";
pub(crate) const NEXT_TAB_ACTION: &str = "next-tab";
pub(crate) const PREVIOUS_TAB_ACTION: &str = "previous-tab";

/// Name of the main window's action that copies the selection (`win.copy`,
/// Ctrl+Shift+C). `window.rs` defines it; the accelerator is in `main.rs`'s
/// `SHORTCUTS`.
pub(crate) const COPY_ACTION: &str = "copy";

/// Name of the main window's action that pastes the clipboard (`win.paste`,
/// Ctrl+Shift+V).
pub(crate) const PASTE_ACTION: &str = "paste";

/// Names of the main window's text size actions (`win.zoom-in`, `win.zoom-out`,
/// `win.zoom-reset`: Ctrl+plus, Ctrl+minus, Ctrl+0, and the ☰ menu's zoom row).
/// `window.rs` defines them; the accelerators are in `main.rs`'s `SHORTCUTS`.
pub(crate) const ZOOM_IN_ACTION: &str = "zoom-in";
pub(crate) const ZOOM_OUT_ACTION: &str = "zoom-out";
pub(crate) const ZOOM_RESET_ACTION: &str = "zoom-reset";

/// The action group each terminal has for itself (`term.`), holding
/// [`MENU_COPY_ACTION`].
const TERMINAL_ACTIONS: &str = "term";

/// The terminal's own Copy (`term.copy`), the one in its right-click menu. It
/// is enabled only while that terminal has a selection, which is what makes the
/// menu entry insensitive without one. It is not the action of the
/// accelerator: `win.copy` stays enabled whenever there is a terminal, so
/// Ctrl+Shift+C never reaches the program ([`copy_selection`]).
const MENU_COPY_ACTION: &str = "copy";

/// Give `terminal` its `term.copy` action ([`MENU_COPY_ACTION`]). The terminal
/// owns the action group and the group's closures hold the terminal weakly, so
/// nothing keeps the terminal alive.
fn add_menu_copy_action(terminal: &Terminal) {
    let copy = gio::SimpleAction::new(MENU_COPY_ACTION, None);
    copy.set_enabled(false);
    copy.connect_activate(glib::clone!(
        #[weak]
        terminal,
        move |_, _| copy_selection_of(&terminal)
    ));
    terminal.connect_selection_changed(glib::clone!(
        #[strong]
        copy,
        move |terminal| copy.set_enabled(terminal.has_selection())
    ));
    let actions = gio::SimpleActionGroup::new();
    actions.add_action(&copy);
    terminal.insert_action_group(TERMINAL_ACTIONS, Some(&actions));
}

/// The menu a terminal shows on right-click: Copy (`term.copy`, insensitive
/// without a selection) and Paste (`win.paste`). VTE shows it on a plain
/// right-click, and on Shift+right-click when the program has turned on mouse
/// reporting (htop, mc, tmux with mouse on), which then gets the plain
/// right-click itself.
pub(crate) fn terminal_menu() -> gio::Menu {
    let menu = gio::Menu::new();
    menu.append(
        Some("Copy"),
        Some(&format!("{TERMINAL_ACTIONS}.{MENU_COPY_ACTION}")),
    );
    menu.append(Some("Paste"), Some(&format!("win.{PASTE_ACTION}")));
    menu
}

/// The terminal of `view`'s selected tab, if that tab shows a session.
fn selected_terminal(view: &adw::TabView) -> Option<Terminal> {
    terminal_of(&view.selected_page()?)
}

/// Copy `terminal`'s selection to the clipboard as plain text; nothing
/// happens without one.
fn copy_selection_of(terminal: &Terminal) {
    if terminal.has_selection() {
        terminal.copy_clipboard_format(Format::Text);
    }
}

/// Copy the selected tab's selection to the clipboard (the action behind
/// Ctrl+Shift+C). With no selection it does nothing, and still counts as
/// handled: the action is enabled whenever there is a terminal, because a
/// disabled one would leave the key to the terminal, where Ctrl+Shift+C may
/// act as Ctrl+C and interrupt the program.
pub(crate) fn copy_selection(view: &adw::TabView) {
    if let Some(terminal) = selected_terminal(view) {
        copy_selection_of(&terminal);
    }
}

/// Paste the clipboard into the selected tab's terminal (the action behind
/// Ctrl+Shift+V and the menu's Paste). It is VTE's own paste, so a program
/// that asked for bracketed paste (shells with it do) gets a multi-line paste
/// as one block instead of running it line by line.
pub(crate) fn paste_clipboard(view: &adw::TabView) {
    if let Some(terminal) = selected_terminal(view) {
        terminal.paste_clipboard();
    }
}

/// The main window's actions that act on its tabs, enabled whenever no dialog
/// is open over the window ([`sync_window_actions`]).
const TAB_WINDOW_ACTIONS: [&str; 4] = [
    NEW_TAB_ACTION,
    CLOSE_TAB_ACTION,
    NEXT_TAB_ACTION,
    PREVIOUS_TAB_ACTION,
];

/// The main window's actions on the selected tab's terminal (Copy, Paste and the
/// three text size actions), enabled while no dialog is open over the window and
/// the selected tab has a terminal ([`sync_window_actions`]).
const TERMINAL_WINDOW_ACTIONS: [&str; 5] = [
    COPY_ACTION,
    PASTE_ACTION,
    ZOOM_IN_ACTION,
    ZOOM_OUT_ACTION,
    ZOOM_RESET_ACTION,
];

/// Enable the main window's tab and terminal actions as the state of `view`'s
/// window allows. Called when the selected tab changes, when a session starts in
/// a tab, and when a dialog opens or closes over the window.
///
/// * The terminal actions (Copy, Paste and the three text size actions) are
///   enabled with a terminal in the selected tab.
/// * While a dialog is open over the window (Rename, About, Keyboard Shortcuts, a
///   confirmation), every one of these actions is disabled, the tab actions
///   (new, close, next, previous) as well. GTK runs application accelerators from
///   the window's capture-phase shortcut controller, before the focused widget,
///   and a dialog is a widget of that window: left enabled, Ctrl+Shift+V would
///   paste into the terminal behind the dialog instead of its text field,
///   Ctrl+plus would resize that terminal, and Ctrl+Shift+W would close its
///   tab. App actions (New Window) are not tab actions and stay as they are.
///
/// A disabled action leaves its accelerator's key to the focused widget, e.g.
/// the new-tab list's search entry, or the field of the dialog. Does nothing for
/// a view outside a main window.
pub(crate) fn sync_window_actions(view: &adw::TabView) {
    let Some(window) = view.root().and_downcast::<adw::ApplicationWindow>() else {
        return;
    };
    let free = window.visible_dialog().is_none();
    let on_terminal = free && selected_terminal(view).is_some();
    let actions = [
        (&TAB_WINDOW_ACTIONS[..], free),
        (&TERMINAL_WINDOW_ACTIONS[..], on_terminal),
    ];
    for (names, enabled) in actions {
        for name in names {
            if let Some(action) = window
                .lookup_action(name)
                .and_downcast::<gio::SimpleAction>()
            {
                action.set_enabled(enabled);
            }
        }
    }
}

/// The text size of the tab of `page`, if it was made by [`new_holder_page`]:
/// the font scale its terminal has, or will have once it shows a session.
pub(crate) fn scale_of(page: &adw::TabPage) -> Option<f64> {
    Some(holder_of(page)?.scale().get())
}

/// Change the text size of the selected tab of `view` from the size it has
/// now, within the range `crate::zoom` allows. Does nothing for a tab that shows
/// no terminal (the new-tab list): the actions are disabled then.
pub(crate) fn zoom_selected(view: &adw::TabView, change: fn(f64) -> f64) {
    let Some(page) = view.selected_page() else {
        return;
    };
    let (Some(holder), Some(terminal)) = (holder_of(&page), terminal_of(&page)) else {
        return;
    };
    let scale = zoom::clamp(change(holder.scale().get()));
    holder.scale().set(scale);
    // One redraw; VTE does the rest.
    terminal.set_font_scale(scale);
}

/// [`new_holder_page_at`] for a view that always has a selected tab to take
/// the size from.
pub(crate) fn new_holder_page(view: &adw::TabView, title: &str) -> adw::TabPage {
    new_holder_page_at(view, title, || zoom::DEFAULT)
}

/// A new, selected page of `view` called `title` that holds nothing yet: its
/// child is an empty box which a chooser or a session ([`start_local_shell`],
/// [`start_profile`], [`start_command`]) later fills. The tab starts at the
/// text size of the tab that is selected in `view` now, or at what
/// `fallback_scale` returns if there is none. (A function, so that the saved
/// size is read from its file only for a window that has no tab to take the
/// size from, not for every tab opened.)
pub(crate) fn new_holder_page_at(
    view: &adw::TabView,
    title: &str,
    fallback_scale: impl FnOnce() -> f64,
) -> adw::TabPage {
    // Before the new page is added and selected.
    let scale = view
        .selected_page()
        .and_then(|page| scale_of(&page))
        .unwrap_or_else(fallback_scale);
    let holder = TabHolder::new(TabTitle::remote(title.to_owned()), scale);
    let page = view.append(&holder);
    refresh_title(&page, &holder);
    view.set_selected_page(&page);
    page
}

/// Show `chooser` in `page`, a fresh page made by [`new_holder_page`] that holds
/// nothing yet: what the tab shows until a session replaces it (the new-tab
/// list).
pub(crate) fn show_chooser(page: &adw::TabPage, chooser: &impl IsA<gtk::Widget>) {
    let holder = holder_of(page).expect("choosers go in pages made by new_holder_page");
    debug_assert!(holder.first_child().is_none(), "a fresh page");
    holder.append(chooser);
}

/// The terminal of `page`, if it holds a session.
pub fn terminal_of(page: &adw::TabPage) -> Option<Terminal> {
    holder_of(page)?.first_child().and_downcast()
}

/// End the session shown in `holder`, whose tab (or window) has closed: take
/// its terminal out, hang up its child ([`hang_up`]) if it is still running,
/// and keep the terminal ([`park`]) until the child's exit is seen, which is
/// when the one-time askpass marker is removed.
///
/// This is done explicitly, not left to the terminal being freed: a closed
/// page's holder is not reliably freed (libadwaita can keep the widget that
/// wraps it alive), and a terminal that lives on keeps its pty open and its
/// child running unseen. The marker is never removed on the strength of the
/// hangup alone: a child that ignores `SIGHUP` (Moniker started under `nohup`)
/// would live on, and a later prompt of its connection must not find the
/// marker gone. A child that has already exited released its marker then.
fn end_session(holder: &TabHolder) {
    let marker = holder.marker().take();
    let terminal = holder.first_child().and_downcast::<Terminal>();
    while let Some(child) = holder.first_child() {
        holder.remove(&child);
    }
    if let Some(terminal) = terminal
        && !holder.child_gone().get()
    {
        hang_up(&terminal);
        park(terminal, marker);
    }
    // Otherwise there is no child, and no marker is left to release: the
    // child's exit released it.
}

thread_local! {
    /// Terminals of closed tabs whose child has been hung up but not seen to
    /// exit yet ([`park`]). GTK runs on one thread.
    static PARKED: RefCell<Vec<Terminal>> = const { RefCell::new(Vec::new()) };
}

/// Keep `terminal`, taken out of its closed tab, until its child exits; then
/// remove `marker` and let the terminal go. While it is kept VTE goes on
/// watching the child (and reaps it). A child that never exits keeps both.
///
/// VTE also reports an exit when a terminal is freed with its child still
/// running, which is not an exit seen; a parked terminal is only freed that
/// way when the thread's storage is torn down at exit, so then the marker
/// stays.
fn park(terminal: Terminal, marker: Option<OneTimeMarker>) {
    let marker = RefCell::new(marker);
    terminal.connect_child_exited(move |terminal, _status| {
        if unpark(terminal)
            && let Some(marker) = marker.take()
        {
            marker.release();
        }
    });
    let _ = PARKED.try_with(|parked| parked.borrow_mut().push(terminal));
}

/// Stop keeping `terminal`. False if the parked terminals are being torn down
/// (the thread is ending).
fn unpark(terminal: &Terminal) -> bool {
    PARKED
        .try_with(|parked| parked.borrow_mut().retain(|t| t != terminal))
        .is_ok()
}

/// Whether `terminal` is kept by [`park`].
fn is_parked(terminal: &Terminal) -> bool {
    PARKED
        .try_with(|parked| parked.borrow().contains(terminal))
        .unwrap_or(false)
}

/// Hang up what runs in `terminal`, as the kernel does when a terminal goes
/// away: `SIGHUP` and then `SIGCONT` (so a stopped job sees it) to the
/// foreground process group of its pty and to the session leader (the shell of
/// a local tab, which may be running a job in a group of its own; `ssh` in a
/// connection tab, with its askpass helper if one is asking).
///
/// Best effort: whether the child really exits is only known from VTE's
/// `child-exited` ([`end_session`] waits for it). Moniker's own process, group
/// and session, and the ids 0 and 1, are never signaled
/// ([`signal_target`]). The group and the session are read from the pty
/// itself, so this cannot reach a process that merely reused the pid of a child
/// that has gone. (Unsetting the pty, which would close it, crashes VTE 0.84
/// while a child runs.)
fn hang_up(terminal: &Terminal) {
    let Some(pty) = terminal.pty() else {
        return;
    };
    // Each fails (no such group, or no session: Errno::OPNOTSUPP or ENOTTY)
    // when nothing runs on the pty any more; then there is nothing to signal.
    // `tcgetpgrp` turns a group of 0 (one outside our pid namespace) into an
    // error itself.
    if let Ok(group) = tcgetpgrp(pty.fd())
        && let Some(group) = signal_target(group.as_raw_pid())
    {
        let _ = kill_process_group(group, Signal::HUP);
        let _ = kill_process_group(group, Signal::CONT);
    }
    if let Some(leader) = pty_session(pty.fd()).and_then(signal_target) {
        let _ = kill_process(leader, Signal::HUP);
        let _ = kill_process(leader, Signal::CONT);
    }
}

/// The session id of the pty `fd`, as the raw number the kernel gives
/// (`TIOCGSID`); none when it fails.
///
/// Not rustix's `tcgetsid`: that wraps the number in a `Pid`, which cannot be
/// zero, without looking, so a 0 (the kernel's answer for a session outside our
/// pid namespace) would panic a debug build and be undefined behavior in a
/// release build before any check of ours could run. Read as a plain integer,
/// the 0 reaches [`signal_target`].
fn pty_session(fd: impl AsFd) -> Option<c_int> {
    // SAFETY: TIOCGSID stores one `pid_t` (a `c_int`) through the pointer it is
    // given and does nothing else, which is what a getter of `c_int` supplies.
    let query = unsafe { ioctl::Getter::<{ libc::TIOCGSID as ioctl::Opcode }, c_int>::new() };
    // SAFETY: as above; the opcode and the output type agree.
    unsafe { ioctl::ioctl(fd, query) }.ok()
}

/// The process (group or session) [`hang_up`] may signal for the raw id `raw`
/// the kernel gave, or none when it must leave it alone: an id of 0 (`kill(0,
/// ..)` would signal Moniker's own process group), 1 (init) or below, or
/// Moniker's own process, group or session. None of them is ever the group or
/// session of a pty Moniker made.
///
/// Checked on the raw number, before a `Pid` (which cannot be zero) is made.
fn signal_target(raw: c_int) -> Option<Pid> {
    if raw <= 1 {
        return None;
    }
    let id = Pid::from_raw(raw)?;
    let ours = id == getpid() || id == getpgrp() || getsid(None).is_ok_and(|s| id == s);
    (!ours).then_some(id)
}

/// Close `page`'s tab, as the close button does: its transcript is written and
/// the window closes if it was the last tab.
fn close_page(page: &adw::TabPage) {
    if let Some(view) = view_of(page) {
        view.close_page(page);
    }
}

/// Empty the holder of `page` and put a new terminal in it, with the automatic
/// title `title` (a name the person gave the tab is kept). The terminal has the
/// focus. Only for a page that has not run a session yet (empty, or showing a
/// chooser): a session replaced here would lose its unwritten transcript.
fn begin_session(page: &adw::TabPage, title: TabTitle) -> (TabHolder, Terminal) {
    let holder = holder_of(page).expect("session tabs are made by new_holder_page");
    holder.title().borrow_mut().adopt(title);
    refresh_title(page, &holder);
    while let Some(old) = holder.first_child() {
        holder.remove(&old);
    }
    let terminal = new_terminal();
    terminal.set_font_scale(holder.scale().get());
    holder.append(&terminal);
    terminal.grab_focus();
    // Connected first, so it is known before anything else hears of the exit.
    terminal.connect_child_exited(glib::clone!(
        #[weak]
        holder,
        move |_, _| holder.child_gone().set(true)
    ));
    // A tab that showed a chooser has a terminal now (Copy, Paste and the
    // text size actions apply).
    if let Some(view) = view_of(page) {
        sync_window_actions(&view);
    }
    (holder, terminal)
}

/// Start `argv` (program first) in `terminal`, in `dir` if given, adding `envv`
/// to the inherited environment. A failure to start is written into the terminal.
fn spawn(terminal: &Terminal, dir: Option<&str>, argv: &[String], envv: &[String]) {
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    let envv: Vec<&str> = envv.iter().map(String::as_str).collect();
    let program = argv.first().copied().unwrap_or_default().to_owned();
    let weak = terminal.downgrade();
    terminal.spawn_async(
        PtyFlags::DEFAULT,
        dir, // working directory: None inherits ours
        &argv,
        &envv,
        // `ssh` is looked up on PATH like any other command.
        glib::SpawnFlags::SEARCH_PATH,
        || {}, // child setup: nothing to do
        -1,    // no timeout
        gio::Cancellable::NONE,
        move |result| {
            // The tab closed while the child was starting: there was nothing on
            // the pty to hang up then, so do it now that there is.
            if result.is_ok()
                && let Some(terminal) = weak.upgrade()
                && is_parked(&terminal)
            {
                hang_up(&terminal);
            }
            if let Err(err) = result {
                let reason: String = err
                    .message()
                    .chars()
                    .map(|c| if c.is_control() { ' ' } else { c })
                    .collect();
                eprintln!("moniker: failed to start {program}: {reason}");
                if let Some(terminal) = weak.upgrade() {
                    show_note(&terminal, &format!("[could not start {program}: {reason}]"));
                    // No child: nothing to wait for when the tab closes.
                    if let Some(holder) = terminal.parent().and_downcast::<TabHolder>() {
                        holder.child_gone().set(true);
                    }
                    unpark(&terminal);
                }
            }
        },
    );
}

/// The shell to run in the local tab: `$SHELL` if it is set to something, else
/// `/bin/sh`. An empty `$SHELL` is as good as none (an empty program name would
/// not start anything).
fn shell_choice(env_shell: Option<&str>) -> String {
    match env_shell {
        Some(shell) if !shell.trim().is_empty() => shell.to_owned(),
        _ => "/bin/sh".to_owned(),
    }
}

/// A new tab with a local shell, at the text size of the tab selected in
/// `view`, or at `fallback_scale` if there is none (the first tab of a window).
/// Closing it, or the shell exiting, closes the tab; the last tab closing
/// closes the window. With a `transcript` the session is saved like a
/// connection's (module docs).
pub fn open_local_shell(
    view: &adw::TabView,
    fallback_scale: f64,
    transcript: Option<TranscriptSpec>,
) -> Terminal {
    let page = new_holder_page_at(view, LOCAL_TITLE, || fallback_scale);
    start_local_shell(&page, transcript)
}

/// Run the person's shell in `page`, in their home directory. The tab is called
/// after what the shell says its title is, and closes when the shell exits. With
/// a `transcript` its text is saved when the shell exits or the tab or window
/// closes.
pub(crate) fn start_local_shell(
    page: &adw::TabPage,
    transcript: Option<TranscriptSpec>,
) -> Terminal {
    let shell = shell_choice(std::env::var("SHELL").ok().as_deref());
    start_shell_in(page, &shell, &glib::home_dir(), transcript)
}

/// [`start_local_shell`] with the shell and the directory given (the tests run a
/// stand-in shell). A directory that is not there leaves the shell in the
/// directory Moniker was started in.
pub(crate) fn start_shell_in(
    page: &adw::TabPage,
    shell: &str,
    dir: &Path,
    transcript: Option<TranscriptSpec>,
) -> Terminal {
    let (holder, terminal) = begin_session(page, TabTitle::local());
    // A shell that exits closes its tab, and closing a tab writes the transcript
    // (`setup_tab_view`), so there is nothing more to do when it exits.
    start_recording(&terminal, transcript);
    terminal.connect_child_exited(glib::clone!(
        #[weak]
        page,
        move |_, _status| close_page(&page)
    ));
    // The shell's own title, e.g. "alice@desktop: ~". Remote tabs never get here:
    // what a remote host says about itself is not to be believed. (VTE 0.78
    // deprecated `window-title` for the `xterm.title` termprop, but the termprop
    // API needs vte4's `v0_78` feature, which would raise the minimum VTE from
    // 0.76; the property is still kept up to date, as the tests check.)
    terminal.connect_window_title_notify(glib::clone!(
        #[weak]
        page,
        #[weak]
        holder,
        move |terminal| {
            let raw = terminal.window_title().unwrap_or_default();
            holder.title().borrow_mut().set_terminal_title(&raw);
            refresh_title(&page, &holder);
        }
    ));
    let dir = dir.is_dir().then(|| dir.to_str()).flatten();
    spawn(&terminal, dir, &[shell.to_owned()], &[]);
    terminal
}

/// Shown as a notification when a password is stored for a profile but the
/// `moniker-askpass` helper is not where it is expected: the tab then asks
/// for the password in the terminal like plain `ssh` does.
pub(crate) const ASKPASS_MISSING_NOTICE: &str = "moniker-askpass not found next to moniker; \
type the password (build with `cargo build --workspace`)";

/// Whether a password is stored for `profile`. A keyring that cannot be reached
/// counts as "no stored password": ssh then asks in the terminal. The keyring
/// error is not logged.
pub(crate) fn has_stored_password(profile: &Profile, secrets: &dyn SecretStore) -> bool {
    matches!(secrets.get(&password_key(&profile.id)), Ok(Some(_)))
}

/// Whether `ssh` can be pointed at the askpass helper at `askpass`: it must be
/// an existing file (a symlink to one counts).
fn askpass_usable(askpass: &Path) -> bool {
    askpass.is_file()
}

/// The notification text when a connection cannot use its stored
/// password because the helper is missing; `None` when there is nothing to say
/// (no password stored, or the helper is there).
pub(crate) fn askpass_notice(has_password: bool, askpass: &Path) -> Option<&'static str> {
    (has_password && !askpass_usable(askpass)).then_some(ASKPASS_MISSING_NOTICE)
}

/// The `ssh` command that connects to `profile`, running its startup sequence
/// number `sequence` if there is one, and the connection's one-time askpass
/// token if it has one.
///
/// `has_password` (see [`has_stored_password`]) decides if `ssh` is pointed at
/// the askpass helper, with a fresh token. If the helper is not an existing
/// file, a stored password cannot be used: the command is the plain one, there
/// is no token, and `ssh` asks in the terminal ([`askpass_notice`] says so).
pub(crate) fn connection_command(
    profile: &Profile,
    sequence: Option<usize>,
    askpass: &Path,
    has_password: bool,
) -> (SshCommand, Option<AskpassToken>) {
    let startup = sequence.and_then(|i| profile.sequences.get(i));
    let token = (has_password && askpass_usable(askpass)).then(AskpassToken::new);
    let command = terminal_command(profile, startup, askpass, token.as_ref());
    (command, token)
}

/// The one-time askpass marker of one connection (`moniker_core::askpass`):
/// where it is and its token, to remove when the connection is over.
///
/// Dropping this removes nothing: only [`release`](OneTimeMarker::release)
/// does, and it may only be called once the connection's `ssh` has exited. A
/// marker left behind is an empty file in the runtime directory (cleared at
/// logout), which is safe; one removed too early would let a living `ssh` get
/// the stored password again.
pub(crate) struct OneTimeMarker {
    runtime_dir: PathBuf,
    token: AskpassToken,
}

impl OneTimeMarker {
    pub(crate) fn new(runtime_dir: PathBuf, token: AskpassToken) -> OneTimeMarker {
        OneTimeMarker { runtime_dir, token }
    }

    /// Remove the marker, if the helper made one (a connection that never got
    /// to a password prompt has nothing to remove). Only once the connection's
    /// `ssh` has been seen to exit (`child-exited`): a hangup alone is not
    /// enough, since `SIGHUP` may be ignored.
    pub(crate) fn release(self) {
        release_marker(&self.runtime_dir, self.token.as_str());
    }
}

/// Everything needed to open the tab for a connection to a profile.
pub(crate) struct Launch {
    pub(crate) title: String,
    pub(crate) command: SshCommand,
    pub(crate) transcript: Option<TranscriptSpec>,
    /// The connection's one-time askpass marker, for the tab to remove when the
    /// session is over. `None` without a token, or without a runtime directory
    /// (then the helper cannot make one either).
    pub(crate) marker: Option<OneTimeMarker>,
}

/// Work out the tab for connecting to `profile` with its startup sequence number
/// `sequence`: its title, the `ssh` command and the transcript settings. `record`
/// says whether the session is recorded (see [`transcript_spec`]). Shows a note
/// as a notification if the stored password cannot be used, in the window of
/// `page`, the tab being opened: a connection started from the Connection
/// Profiles window opens its tab in a main window, and that is where the note
/// belongs.
pub(crate) fn launch(
    page: &adw::TabPage,
    profile: &Profile,
    sequence: Option<usize>,
    record: bool,
    ctx: &AppContext,
    transcript_dir: &Path,
) -> Launch {
    let has_password = has_stored_password(profile, ctx.secrets.as_ref());
    if let Some(note) = askpass_notice(has_password, &ctx.askpass_path) {
        ctx.notice.show_near(&page.child(), note);
    }
    let (command, token) = connection_command(profile, sequence, &ctx.askpass_path, has_password);
    let marker = token
        .zip(ctx.runtime_dir.clone())
        .map(|(token, dir)| OneTimeMarker::new(dir, token));
    Launch {
        title: tab_title(profile),
        command,
        transcript: transcript_spec(profile, record, transcript_dir, &ctx.notice),
        marker,
    }
}

/// Connect to `profile` in a new tab, running its startup sequence number
/// `sequence` (an index into `profile.sequences`) if given, and recording a
/// transcript if `record` is set. Callers that make no choice of their own pass
/// what [`default_save`] gives for the profile's own setting and "Save
/// transcripts of all sessions" (not `profile.transcript` alone, which would
/// ignore the latter). The tab starts at the text size of the tab selected in
/// `view`, or, in a window with none, at the size saved for the next launch.
pub fn open_profile(
    view: &adw::TabView,
    ctx: &AppContext,
    profile: &Profile,
    sequence: Option<usize>,
    record: bool,
) {
    let page = new_holder_page_at(view, &tab_title(profile), || ctx.saved_font_scale());
    start_profile(&page, ctx, profile, sequence, record);
}

/// [`open_profile`] in the page `page`, which [`new_holder_page`] made.
pub(crate) fn start_profile(
    page: &adw::TabPage,
    ctx: &AppContext,
    profile: &Profile,
    sequence: Option<usize>,
    record: bool,
) {
    let Launch {
        title,
        command,
        transcript,
        marker,
    } = launch(page, profile, sequence, record, ctx, &ctx.transcripts_dir());
    start_command(
        page,
        &title,
        test_hook::command(command),
        transcript,
        marker,
    );
}

/// Tests only: the program [`start_profile`] runs in place of the system `ssh`,
/// so a session started through the real paths (the new-tab list) runs a fake.
#[cfg(test)]
pub(crate) fn set_test_ssh_program(program: Option<String>) {
    test_hook::PROGRAM.replace(program);
}

/// Where [`set_test_ssh_program`] takes effect. Outside tests the command is
/// passed on untouched.
mod test_hook {
    use moniker_core::ssh::SshCommand;

    #[cfg(test)]
    thread_local! {
        pub(super) static PROGRAM: std::cell::RefCell<Option<String>> =
            const { std::cell::RefCell::new(None) };
    }

    #[cfg(test)]
    pub(super) fn command(mut cmd: SshCommand) -> SshCommand {
        if let Some(program) = PROGRAM.with_borrow(Clone::clone) {
            cmd.program = program;
        }
        cmd
    }

    #[cfg(not(test))]
    pub(super) fn command(cmd: SshCommand) -> SshCommand {
        cmd
    }
}

/// Run `cmd` in a new tab called `title`; the tests start a fake `ssh` this way.
/// (The app itself only fills pages that [`new_holder_page`] made, with
/// [`start_command`] through [`start_profile`].)
///
/// With a `transcript`, the terminal keeps unlimited scrollback and its text is
/// written when the session ends, as described in the module docs.
#[cfg(test)]
pub(crate) fn open_command(
    view: &adw::TabView,
    title: &str,
    cmd: SshCommand,
    transcript: Option<TranscriptSpec>,
) -> Terminal {
    let page = new_holder_page(view, title);
    start_command(&page, title, cmd, transcript, None)
}

/// [`open_command`] in the page `page`, which [`new_holder_page`] made. The tab
/// is called `title` and never takes another from the program it runs.
///
/// `marker` is the connection's one-time askpass marker: the tab keeps it and
/// removes it when the child exits or the tab or window closes (module docs).
pub(crate) fn start_command(
    page: &adw::TabPage,
    title: &str,
    cmd: SshCommand,
    transcript: Option<TranscriptSpec>,
    marker: Option<OneTimeMarker>,
) -> Terminal {
    let (holder, terminal) = begin_session(page, TabTitle::remote(title.to_owned()));
    holder.marker().replace(marker);
    let session = start_recording(&terminal, transcript);

    terminal.connect_child_exited(glib::clone!(
        #[weak]
        page,
        #[weak]
        holder,
        #[strong]
        session,
        move |terminal, status| {
            // `ssh` is gone: no prompt of this connection can come any more.
            if let Some(marker) = holder.marker().take() {
                marker.release();
            }
            if let Some(session) = &session {
                session.write_transcript_once();
            }
            match exit_action(status) {
                ExitAction::Close => close_page(&page),
                ExitAction::Keep { note } => show_note(terminal, &note),
            }
        }
    ));

    let (argv, envv) = spawn_args(&cmd);
    spawn(&terminal, None, &argv, &envv);
    terminal
}

#[cfg(test)]
mod tests {
    use super::*;
    use moniker_core::profile::StartupSequence;
    use moniker_core::ssh::terminal_command;

    fn profile() -> Profile {
        Profile::new("Prod web", "secret-host.example.com", "alice")
    }

    fn cmd(args: &[&str], env: &[(&str, &str)]) -> SshCommand {
        SshCommand {
            program: "ssh".to_owned(),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            env: env
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        }
    }

    // spawn_args

    #[test]
    fn argv_is_the_program_then_each_argument() {
        let (argv, env) = spawn_args(&cmd(&["-p", "22", "--", "alice@h"], &[]));
        assert_eq!(argv, ["ssh", "-p", "22", "--", "alice@h"]);
        assert!(env.is_empty());
    }

    #[test]
    fn env_entries_are_key_equals_value() {
        let (_, env) = spawn_args(&cmd(
            &[],
            &[
                ("SSH_ASKPASS", "/opt/a b/askpass"),
                ("EMPTY", ""),
                ("EQ", "a=b"),
            ],
        ));
        assert_eq!(env, ["SSH_ASKPASS=/opt/a b/askpass", "EMPTY=", "EQ=a=b"]);
    }

    #[test]
    fn a_remote_command_stays_one_argument_with_its_newline_and_dollar() {
        let seq = StartupSequence {
            name: "update".to_owned(),
            commands: vec!["uptime".to_owned(), "echo $HOME".to_owned()],
        };
        let p = profile();
        let token = AskpassToken::new();
        let ssh = terminal_command(&p, Some(&seq), Path::new("/x/askpass"), Some(&token));
        let (argv, env) = spawn_args(&ssh);
        let remote = "uptime\necho $HOME\nexec \"$SHELL\" -l";
        assert_eq!(argv.last().map(String::as_str), Some(remote));
        assert_eq!(argv.iter().filter(|a| a.contains('\n')).count(), 1);
        assert_eq!(
            env,
            [
                "SSH_ASKPASS=/x/askpass".to_owned(),
                "SSH_ASKPASS_REQUIRE=force".to_owned(),
                format!("MONIKER_PROFILE_ID={}", p.id),
                format!("MONIKER_PID={}", std::process::id()),
                format!("MONIKER_ASKPASS_TOKEN={}", token.as_str()),
            ]
        );
    }

    // exit_action

    fn keep(note: &str) -> ExitAction {
        ExitAction::Keep {
            note: note.to_owned(),
        }
    }

    #[test]
    fn exit_status_zero_closes_the_tab() {
        assert_eq!(exit_action(0), ExitAction::Close);
    }

    #[test]
    fn a_nonzero_exit_code_keeps_the_tab() {
        // The wait status holds the exit code in its second byte.
        assert_eq!(exit_action(1 << 8), keep("[session ended: status 1]"));
        assert_eq!(exit_action(255 << 8), keep("[session ended: status 255]"));
    }

    #[test]
    fn a_signal_keeps_the_tab() {
        assert_eq!(exit_action(9), keep("[session ended: signal 9]"));
        // SIGSEGV with the core-dumped bit set.
        assert_eq!(exit_action(0x80 | 11), keep("[session ended: signal 11]"));
    }

    #[test]
    fn a_status_that_is_neither_keeps_the_tab() {
        // 0x7f in the low byte is "stopped", not an exit.
        assert_eq!(exit_action(0x137f), keep("[session ended: status 4991]"));
    }

    // tab_title

    #[test]
    fn the_title_is_the_profile_name_only() {
        let title = tab_title(&profile());
        assert_eq!(title, "Prod web");
        assert!(!title.contains("secret-host"));
        assert!(!title.contains("alice"));
    }

    #[test]
    fn markup_in_a_name_is_left_alone() {
        let mut p = profile();
        p.name = "<b>web</b> & _x_".to_owned();
        assert_eq!(tab_title(&p), "<b>web</b> & _x_");
    }

    #[test]
    fn control_characters_in_a_name_become_spaces() {
        let mut p = profile();
        p.name = "  web\n\tone\r\u{1b}[31m ".to_owned();
        assert_eq!(tab_title(&p), "web  one  [31m");
    }

    #[test]
    fn a_name_of_only_whitespace_gets_a_fallback_title() {
        let mut p = profile();
        p.name = " \n ".to_owned();
        assert_eq!(tab_title(&p), "Session");
    }

    // clean_title and TabTitle

    #[test]
    fn titles_lose_control_characters() {
        assert_eq!(
            clean_title("a\x1b]0;evil\x07b").as_deref(),
            Some("a ]0;evil b")
        );
        assert_eq!(clean_title("\n\r"), None);
        assert_eq!(clean_title("  plain  ").as_deref(), Some("plain"));
    }

    #[test]
    fn a_remote_tab_ignores_the_shell_title() {
        let mut t = TabTitle::remote("web-01".into());
        t.set_terminal_title("root@other-host");
        assert_eq!(t.shown(), "web-01");
    }

    #[test]
    fn a_local_tab_follows_its_shell_until_renamed() {
        let mut t = TabTitle::local();
        assert_eq!(t.shown(), "Local terminal");
        t.set_terminal_title("justin@desktop: ~");
        assert_eq!(t.shown(), "justin@desktop: ~");
        t.rename("build");
        t.set_terminal_title("justin@desktop: /tmp");
        assert_eq!(t.shown(), "build");
    }

    #[test]
    fn renaming_to_blank_restores_the_automatic_title() {
        let mut t = TabTitle::remote("web-01".into());
        t.rename("prod");
        t.rename(" \t\n");
        assert_eq!(t.shown(), "web-01");

        // A local tab goes back to what its shell last said.
        let mut t = TabTitle::local();
        t.set_terminal_title("justin@desktop: ~");
        t.rename("build");
        t.set_terminal_title("justin@desktop: /tmp");
        t.rename("");
        assert_eq!(t.shown(), "justin@desktop: /tmp");
    }

    #[test]
    fn a_local_tab_whose_shell_clears_the_title_gets_the_default_back() {
        let mut t = TabTitle::local();
        t.set_terminal_title("justin@desktop: ~");
        t.set_terminal_title("");
        assert_eq!(t.shown(), "Local terminal");
        t.set_terminal_title("\x1b\x07");
        assert_eq!(t.shown(), "Local terminal");
    }

    #[test]
    fn titles_given_to_a_tab_are_cleaned() {
        let mut t = TabTitle::remote("web\n01".into());
        assert_eq!(t.shown(), "web 01");
        t.rename("a\x1b[31mb");
        assert_eq!(t.shown(), "a [31mb");
        assert_eq!(TabTitle::remote(" \n".into()).shown(), "Session");
    }

    #[test]
    fn a_tab_that_becomes_a_session_keeps_the_name_it_was_given() {
        // A chooser tab renamed before a profile was picked.
        let mut t = TabTitle::remote("New Tab".into());
        t.rename("mine");
        t.adopt(TabTitle::remote("web-01".into()));
        assert_eq!(t.shown(), "mine");
        t.rename("");
        assert_eq!(t.shown(), "web-01");

        // And one that was not renamed takes the session's title, and then
        // follows or ignores its shell as the session says.
        let mut t = TabTitle::remote("New Tab".into());
        t.adopt(TabTitle::local());
        assert_eq!(t.shown(), "Local terminal");
        t.set_terminal_title("justin@desktop: ~");
        assert_eq!(t.shown(), "justin@desktop: ~");
        t.adopt(TabTitle::remote("web-01".into()));
        t.set_terminal_title("root@other-host");
        assert_eq!(t.shown(), "web-01");
    }

    // transcript_spec

    #[test]
    fn no_spec_when_the_session_does_not_record() {
        assert!(transcript_spec(&profile(), false, Path::new("/t"), &Notice::default()).is_none());
    }

    #[test]
    fn spec_names_the_host_and_directory_when_it_records() {
        let mut p = profile();
        p.transcript = true;
        let notice = Notice::default();
        let spec = transcript_spec(&p, true, Path::new("/t"), &notice).expect("a spec");
        assert_eq!(spec.host, "secret-host.example.com");
        assert_eq!(spec.dir, Path::new("/t"));
        assert!(spec.notice == notice, "reports to the given notice board");
    }

    #[test]
    fn default_save_truth_table() {
        // (profile's own setting, save all) -> checked
        assert!(!default_save(false, false), "nothing asks for it");
        assert!(default_save(true, false), "the profile's own setting");
        assert!(
            default_save(false, true),
            "save all beats a profile that saves nothing"
        );
        assert!(default_save(true, true));
    }

    #[test]
    fn a_machine_with_no_name_is_localhost() {
        assert_eq!(node_name_or_localhost(""), "localhost");
        assert_eq!(node_name_or_localhost("desktop"), "desktop");
        // Whatever this machine is called, it is something to name a file after.
        assert!(!local_host_name().is_empty());
    }

    #[test]
    fn the_connect_time_choice_overrides_the_profile_setting_both_ways() {
        let notice = Notice::default();
        // Profile off, record on: a transcript is made.
        let mut off = profile();
        off.transcript = false;
        let spec = transcript_spec(&off, true, Path::new("/t"), &notice).expect("a spec");
        assert_eq!(spec.host, off.host);
        // Profile on, record off: none is made.
        let mut on = profile();
        on.transcript = true;
        assert!(transcript_spec(&on, false, Path::new("/t"), &notice).is_none());
        // Agreeing with the profile changes nothing.
        assert!(transcript_spec(&off, false, Path::new("/t"), &notice).is_none());
        assert!(transcript_spec(&on, true, Path::new("/t"), &notice).is_some());
    }

    // shell_choice

    #[test]
    fn the_shell_comes_from_the_environment_when_it_names_one() {
        assert_eq!(shell_choice(Some("/usr/bin/zsh")), "/usr/bin/zsh");
    }

    #[test]
    fn an_unset_or_empty_shell_falls_back_to_bin_sh() {
        assert_eq!(shell_choice(None), "/bin/sh");
        assert_eq!(shell_choice(Some("")), "/bin/sh");
        assert_eq!(shell_choice(Some("  \t")), "/bin/sh");
    }

    // connection_command

    use moniker_core::secrets::{MemorySecretStore, SecretError};
    use zeroize::Zeroizing;

    /// A keyring that cannot be reached.
    struct BrokenKeyring;

    impl SecretStore for BrokenKeyring {
        fn get(&self, _: &str) -> Result<Option<Zeroizing<String>>, SecretError> {
            Err(SecretError::Unavailable("no bus".to_owned()))
        }
        fn set(&self, _: &str, _: &str) -> Result<(), SecretError> {
            Err(SecretError::Unavailable("no bus".to_owned()))
        }
        fn delete(&self, _: &str) -> Result<(), SecretError> {
            Err(SecretError::Unavailable("no bus".to_owned()))
        }
        fn probe(&self) -> Result<(), SecretError> {
            Err(SecretError::Unavailable("no bus".to_owned()))
        }
    }

    fn with_sequences() -> Profile {
        let mut p = profile();
        p.sequences = vec![
            StartupSequence {
                name: "first".to_owned(),
                commands: vec!["uptime".to_owned()],
            },
            StartupSequence {
                name: "second".to_owned(),
                commands: vec!["df -h".to_owned()],
            },
        ];
        p
    }

    /// A stand-in for the helper: an existing file in a directory of its own.
    struct Helper {
        _dir: tempfile::TempDir,
        path: PathBuf,
    }

    fn helper() -> Helper {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("moniker-askpass");
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        Helper { _dir: dir, path }
    }

    /// Where the helper is expected but is not.
    fn missing_helper() -> &'static Path {
        Path::new("/nonexistent/moniker/moniker-askpass")
    }

    #[test]
    fn a_stored_password_points_ssh_at_the_askpass_helper() {
        let p = profile();
        let h = helper();
        let secrets = MemorySecretStore::default();
        secrets.set(&password_key(&p.id), "hunter2").unwrap();
        let (ssh, token) = connection_command(&p, None, &h.path, has_stored_password(&p, &secrets));
        let token = token.expect("a one-time token");
        let (argv, env) = spawn_args(&ssh);
        assert_eq!(
            env,
            [
                format!("SSH_ASKPASS={}", h.path.display()),
                "SSH_ASKPASS_REQUIRE=force".to_owned(),
                format!("MONIKER_PROFILE_ID={}", p.id),
                format!("MONIKER_PID={}", std::process::id()),
                format!("MONIKER_ASKPASS_TOKEN={}", token.as_str()),
            ]
        );
        // The password itself is nowhere in what is handed to the child.
        assert!(argv.iter().chain(&env).all(|e| !e.contains("hunter2")));
        // ssh's usual three password prompts: the helper sends the stored
        // password for one of them at most.
        assert!(argv.iter().all(|a| !a.contains("NumberOfPasswordPrompts")));
    }

    #[test]
    fn every_connection_gets_a_fresh_token() {
        let p = profile();
        let h = helper();
        let (_, a) = connection_command(&p, None, &h.path, true);
        let (_, b) = connection_command(&p, None, &h.path, true);
        assert_ne!(a.expect("a token"), b.expect("a token"));
    }

    #[test]
    fn only_release_removes_the_marker() {
        use moniker_core::askpass::{claim_marker, marker_dir};
        let runtime = tempfile::tempdir().unwrap();
        let token = AskpassToken::new();
        assert!(claim_marker(runtime.path(), token.as_str()).unwrap());
        let path = marker_dir(runtime.path()).join(token.as_str());
        // Dropped unreleased (the session may not be over): it stays.
        drop(OneTimeMarker::new(
            runtime.path().to_path_buf(),
            token.clone(),
        ));
        assert!(path.exists());
        OneTimeMarker::new(runtime.path().to_path_buf(), token).release();
        assert!(!path.exists());
    }

    #[test]
    fn no_stored_password_means_no_askpass_environment() {
        let h = helper();
        let p = profile();
        let none = has_stored_password(&p, &MemorySecretStore::default());
        assert!(!none);
        let (ssh, token) = connection_command(&p, None, &h.path, none);
        assert!(ssh.env.is_empty());
        assert!(token.is_none());
    }

    #[test]
    fn an_unreachable_keyring_counts_as_no_password() {
        let h = helper();
        let p = profile();
        let none = has_stored_password(&p, &BrokenKeyring);
        assert!(!none);
        let (ssh, token) = connection_command(&p, None, &h.path, none);
        assert!(ssh.env.is_empty());
        assert!(token.is_none());
        assert_eq!(ssh.program, "ssh");
    }

    #[test]
    fn a_missing_helper_means_no_askpass_environment_even_with_a_stored_password() {
        let p = profile();
        let secrets = MemorySecretStore::default();
        secrets.set(&password_key(&p.id), "hunter2").unwrap();
        let has_password = has_stored_password(&p, &secrets);
        assert!(has_password);

        let (plain, token) = connection_command(&p, None, missing_helper(), has_password);
        assert!(
            plain.env.is_empty(),
            "ssh must not be pointed at a missing file"
        );
        assert!(token.is_none());
        assert_eq!(plain.program, "ssh");
        // The rest of the command is the ordinary one.
        let h = helper();
        let (with_helper, _) = connection_command(&p, None, &h.path, false);
        assert_eq!(plain.args, with_helper.args);

        // A directory is not a helper either.
        let dir = tempfile::tempdir().unwrap();
        let (ssh, token) = connection_command(&p, None, dir.path(), has_password);
        assert!(ssh.env.is_empty());
        assert!(token.is_none());

        // The startup sequence still runs.
        let q = with_sequences();
        let (ssh, _) = connection_command(&q, Some(0), missing_helper(), true);
        assert!(ssh.env.is_empty());
        assert_eq!(
            ssh.args.last().map(String::as_str),
            Some("uptime\nexec \"$SHELL\" -l")
        );
    }

    #[test]
    fn a_symlink_to_the_helper_is_a_helper() {
        let h = helper();
        let link_dir = tempfile::tempdir().unwrap();
        let link = link_dir.path().join("moniker-askpass");
        std::os::unix::fs::symlink(&h.path, &link).unwrap();
        let (ssh, token) = connection_command(&profile(), None, &link, true);
        assert_eq!(ssh.env.len(), 5);
        assert!(token.is_some());
        // A dangling one is not.
        std::fs::remove_file(&h.path).unwrap();
        let (ssh, token) = connection_command(&profile(), None, &link, true);
        assert!(ssh.env.is_empty());
        assert!(token.is_none());
    }

    #[test]
    fn the_notice_appears_only_for_a_stored_password_and_a_missing_helper() {
        let h = helper();
        let notice = askpass_notice(true, missing_helper()).expect("a notice");
        assert_eq!(notice, ASKPASS_MISSING_NOTICE);
        assert!(notice.contains("moniker-askpass not found next to moniker"));
        assert!(notice.contains("type the password"));
        assert!(notice.contains("cargo build --workspace"));
        assert_eq!(askpass_notice(true, &h.path), None);
        // Nothing stored: ssh asks in the terminal anyway, nothing to explain.
        assert_eq!(askpass_notice(false, missing_helper()), None);
        assert_eq!(askpass_notice(false, &h.path), None);
    }

    #[test]
    fn the_chosen_sequence_becomes_the_remote_command() {
        let p = with_sequences();
        let h = helper();
        let (second, _) = connection_command(&p, Some(1), &h.path, false);
        assert_eq!(
            second.args.last().map(String::as_str),
            Some("df -h\nexec \"$SHELL\" -l")
        );
        assert!(second.args.contains(&"-t".to_owned()));
        let (first, _) = connection_command(&p, Some(0), &h.path, false);
        assert_eq!(
            first.args.last().map(String::as_str),
            Some("uptime\nexec \"$SHELL\" -l")
        );
    }

    #[test]
    fn just_connect_and_a_stale_index_run_no_sequence() {
        let p = with_sequences();
        let h = helper();
        for choice in [None, Some(2)] {
            let (ssh, _) = connection_command(&p, choice, &h.path, false);
            assert_eq!(
                ssh.args.last().map(String::as_str),
                Some("alice@secret-host.example.com")
            );
            assert!(!ssh.args.contains(&"-t".to_owned()));
        }
    }

    // note_bytes

    #[test]
    fn a_note_starts_a_new_line_when_the_cursor_is_mid_line() {
        assert_eq!(note_bytes("[x]", false), b"\r\n[x]\r\n");
    }

    #[test]
    fn a_note_follows_output_that_ended_with_a_newline_directly() {
        assert_eq!(note_bytes("[x]", true), b"[x]\r\n");
    }

    // transcript_body

    #[test]
    fn blank_rows_below_the_output_are_dropped() {
        assert_eq!(
            transcript_body("$ uptime\n 10:00 up\n$ \n\n\n  \n"),
            "$ uptime\n 10:00 up\n$\n"
        );
    }

    #[test]
    fn blank_rows_inside_the_output_are_kept() {
        assert_eq!(transcript_body("a\n\nb"), "a\n\nb\n");
    }

    #[test]
    fn leading_indentation_is_kept() {
        assert_eq!(transcript_body("  indented\n"), "  indented\n");
    }

    #[test]
    fn an_empty_screen_gives_an_empty_body() {
        assert_eq!(transcript_body(""), "");
        assert_eq!(transcript_body("\n \n\n"), "");
    }

    // signal_target

    #[test]
    fn zero_one_and_below_are_never_signaled() {
        for raw in [0, 1, -1, c_int::MIN] {
            assert_eq!(signal_target(raw), None, "{raw}");
        }
    }

    #[test]
    fn moniker_itself_is_never_signaled() {
        assert_eq!(signal_target(getpid().as_raw_pid()), None);
        assert_eq!(signal_target(getpgrp().as_raw_pid()), None);
        let session = getsid(None).expect("our session");
        assert_eq!(signal_target(session.as_raw_pid()), None);
    }

    #[test]
    fn another_process_may_be_signaled() {
        let other = Pid::from_raw(c_int::MAX).expect("a pid");
        assert_eq!(signal_target(c_int::MAX), Some(other));
        // The one after init is as good as any other.
        assert_eq!(signal_target(2).map(Pid::as_raw_pid), Some(2));
    }

    // transcript_notice

    #[test]
    fn a_saved_transcript_says_nothing() {
        assert_eq!(transcript_notice(&Ok(PathBuf::from("/t/x.txt"))), None);
    }

    #[test]
    fn a_failed_transcript_is_reported() {
        let err = io::Error::other("disk full");
        assert_eq!(
            transcript_notice(&Err(err)).as_deref(),
            Some("Could not save the transcript: disk full")
        );
    }
}
