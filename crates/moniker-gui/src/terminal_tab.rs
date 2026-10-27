//! Terminal tabs: the local shell and SSH connections made from profiles.
//!
//! The tabs are the pages of an `adw::TabView`; [`setup_tab_view`] and
//! [`setup_tab_bar`] wire one up. A tab holds one pane or several
//! (`crate::panes`), and each pane one session: its holder ([`PaneHolder`]), an
//! empty box that later receives a chooser or a session. A page starts with one
//! empty pane ([`new_holder_page`]); [`start_local_in`], [`start_profile`]
//! and [`start_command`] put a terminal in a pane, and the `open_*` functions
//! are a new holder page followed by one of those. Everything below that is
//! said of a tab holds for each pane of a tab with several: its own session,
//! title, text size, transcript and askpass marker. The tab, the window's
//! actions and the ☰ menu go by the tab's focused pane. The holder also keeps
//! what its pane shows ([`PaneKind`]), which a saved session records
//! (`crate::session_layout`, issue #12), with the folder a local shell last
//! reported ([`current_folder`]).
//!
//! [`open_profile`] turns a profile into an `ssh` command with
//! `moniker_core::ssh::terminal_command` and runs it in a VTE tab. The command
//! is passed to VTE as an argv list and `KEY=VALUE` entries, never as a shell
//! string: the startup-sequence argument holds newlines and a literal `$SHELL`
//! that must reach the remote shell untouched. VTE adds the entries to the
//! inherited environment; it does not replace it.
//!
//! When a connection ends with a clean exit its pane closes ([`close_pane`]:
//! the tab, when it was its only pane). After a failure (non-zero status, or a
//! signal) the pane stays open with a short note, so the `ssh` error message
//! can still be read. Closing the last tab of a window closes the window.
//!
//! # Local tabs and commands
//!
//! A local tab ([`open_local`]) runs the person's shell, or a command given on
//! the command line, in a folder, as a [`Start`] says. A launch handed to a
//! Moniker that is already running passes no environment on: the terminal gets
//! that Moniker's own (GApplication's `SEND_ENVIRONMENT` flag is not set). The
//! launch that starts Moniker is different: the launching process is Moniker
//! then, so it has its launcher's environment, as any program has, and every
//! terminal of that run inherits it. Nothing here reaches a profile, a password
//! or a remote session. A command
//! ([`start_command_in`]) is an argument list, never a shell string, and is named
//! after its program until it sets a title; a title given with it is fixed as a
//! rename is. When it ends, success closes the tab and any other end keeps it
//! with the note a connection shows ([`exit_action`]); `hold` keeps it with
//! [`hold_note`] on success too. One that cannot be started is a "[could not
//! start …]" note in the tab, as a connection that cannot is.
//!
//! Local terminal has startup sequences of its own (issue #13,
//! `crate::local_terminal`, never synced). One runs when Local terminal is
//! picked ([`start_local_in`]: its default for a plain pick in the new-tab list
//! and for a pane a saved session adds back, the one chosen under "›"
//! otherwise; [`open_local_pick`] for a new tab), never in the first tab of a
//! window. Its commands are the person's own, run by their shell as
//! `shell -c <script>` ([`start_sequence_in`]), with the shell's path in the
//! environment only; the script ends by `exec`ing that shell, so the tab is an
//! interactive shell afterwards. Such a tab ends as a command tab does, not as
//! a plain shell: success closes it, any other end keeps it with the note.
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
//! insensitive without one. Its Paste is the terminal's own `term.paste`, so a
//! right click in a pane that does not have the focus pastes there, and its
//! Split Right and Split Down split that pane.
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
//! setting and a local tab follows Local terminal's own "Save transcript"
//! switch (issue #13), which is off unless the person turned it on.
//! Either way the person can choose otherwise for one session with the "Save
//! transcript" box in the choices behind "›" in the new-tab list
//! ([`transcript_spec`], [`local_transcript_spec`]). The folder a session saves
//! to is the one in the settings when the session starts, until the person
//! chooses another folder in the Preferences: then every session still open is
//! pointed at the new folder before the old one's transcripts move there
//! ([`save_open_transcripts_to`]).
//! While a session records, its pane says so ([`PaneHolder::recording`]): its
//! tab shows a record icon and REC before its name, as do the header bar for
//! the tab in front and the pane's header in a split (`crate::panes`,
//! `crate::window`), from the moment it starts recording until its transcript
//! is written. REC is never part of the name.
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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use adw::prelude::*;
use gtk::{gio, glib};
use gtk4 as gtk;
use libadwaita as adw;
use moniker_core::askpass::{AskpassToken, release_marker};
use moniker_core::profile::{Profile, StartupSequence};
use moniker_core::secrets::{SecretStore, password_key};
use moniker_core::ssh::{SshCommand, local_command, terminal_command};
use moniker_core::transcript::{transcript_path, write_transcript};
use rustix::ioctl;
use rustix::process::{Pid, Signal, getpgrp, getpid, getsid, kill_process, kill_process_group};
use rustix::termios::tcgetpgrp;
use vte4::prelude::*;
use vte4::{Format, PtyFlags, Terminal};

use crate::context::{AppContext, Notice};
use crate::launch_args::{Run, Start, one_line};
use crate::local_terminal::LocalTerminal;
use crate::settings::{self, GuiSettings};
use crate::{panes, theme, zoom};

pub(crate) use self::holder::PaneHolder;

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
    if ExitStatus::from_raw(status).code() == Some(0) {
        return ExitAction::Close;
    }
    ExitAction::Keep {
        note: hold_note(status),
    }
}

/// The note a session that has ended leaves in its terminal, for the raw wait
/// status `status`: the exit code, or the signal that ended it. [`exit_action`]
/// writes exactly this for every end but a clean one, and a held command
/// ([`start_command_in`] with `hold`) writes it for that one too, as
/// "[session ended: status 0]".
pub fn hold_note(status: i32) -> String {
    let wait = ExitStatus::from_raw(status);
    match (wait.code(), wait.signal()) {
        (Some(code), _) => format!("[session ended: status {code}]"),
        (None, Some(signal)) => format!("[session ended: signal {signal}]"),
        // Stopped or unknown: not a clean exit either.
        (None, None) => format!("[session ended: status {status}]"),
    }
}

/// What a command tab does when its command ends: as a connection does, or
/// with `hold` keeps the tab whatever the status was.
fn command_exit_action(status: i32, hold: bool) -> ExitAction {
    if hold {
        ExitAction::Keep {
            note: hold_note(status),
        }
    } else {
        exit_action(status)
    }
}

/// The automatic title of a tab that runs the command whose program is
/// `argv0`: the program's file name (`/usr/bin/htop` is "htop"), until the
/// program sets a title of its own. A name with nothing to show (empty, or only
/// control characters) is the local tab's.
fn program_title(argv0: &str) -> String {
    let name = Path::new(argv0)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(argv0);
    clean_title(name).unwrap_or_else(|| LOCAL_TITLE.to_owned())
}

/// `raw` as a one-line title: control characters (an escape sequence in a title
/// set by a program, a newline in a name) become spaces and the ends are trimmed.
/// `None` if nothing is left.
pub fn clean_title(raw: &str) -> Option<String> {
    let flat = one_line(raw);
    let title = flat.trim();
    (!title.is_empty()).then(|| title.to_owned())
}

/// The note a program that could not be started leaves in its terminal. The
/// program is whatever the command line named (any `-e` argument) and the
/// reason is GLib's, which quotes it, so both go through [`one_line`]: the note
/// is fed to the terminal, which would act on an escape sequence or a line
/// break in either.
fn start_failure_note(program: &str, reason: &str) -> String {
    format!(
        "[could not start {}: {}]",
        one_line(program),
        one_line(reason)
    )
}

/// The same failure as a line for standard error, which is often the terminal
/// that started Moniker and would act on an escape sequence just as well.
fn start_failure_log(program: &str, reason: &str) -> String {
    format!(
        "moniker: failed to start {}: {}",
        one_line(program),
        one_line(reason)
    )
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
    /// What a program that clears its title gets back as the automatic one.
    fallback: String,
    /// Whether [`set_terminal_title`](TabTitle::set_terminal_title) changes the
    /// automatic title: only a local tab listens to its shell.
    follows_shell: bool,
    renamed: Option<String>,
}

impl TabTitle {
    /// The title of a tab connected to a profile titled `profile_title`. The
    /// remote shell cannot change it.
    pub fn remote(profile_title: String) -> TabTitle {
        let automatic = clean_title(&profile_title).unwrap_or_else(|| FALLBACK_TITLE.to_owned());
        TabTitle {
            fallback: automatic.clone(),
            automatic,
            follows_shell: false,
            renamed: None,
        }
    }

    /// The title of a local tab: "Local terminal" until its shell sets one.
    pub fn local() -> TabTitle {
        TabTitle::following(LOCAL_TITLE.to_owned())
    }

    /// The title of a local tab that runs the command whose program is
    /// `argv0`: the program's file name ([`program_title`]) until the program
    /// sets one, and again if it clears it.
    fn program(argv0: &str) -> TabTitle {
        TabTitle::following(program_title(argv0))
    }

    /// A local tab's title: `name` until the program in the tab sets one.
    fn following(name: String) -> TabTitle {
        TabTitle {
            automatic: name.clone(),
            fallback: name,
            follows_shell: true,
            renamed: None,
        }
    }

    /// The title the shell in the tab asked for (`raw` is as VTE reports it).
    /// Only a local tab takes it, and a program that clears its title gets the
    /// tab's default back.
    pub fn set_terminal_title(&mut self, raw: &str) {
        if self.follows_shell {
            self.automatic = clean_title(raw).unwrap_or_else(|| self.fallback.clone());
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

    /// The name the person gave the tab, if any (never empty).
    pub fn renamed(&self) -> Option<&str> {
        self.renamed.as_deref()
    }

    /// Make `next` the automatic title, as when a tab that held a chooser turns
    /// into a session. A name the person already gave the tab is kept.
    fn adopt(&mut self, next: TabTitle) {
        let renamed = self.renamed.take();
        *self = TabTitle { renamed, ..next };
    }
}

/// What a pane shows, as a saved session records it (issue #12): set where the
/// pane is filled ([`show_chooser`], [`start_shell_in`], [`start_sequence_in`],
/// [`start_command_in`], [`start_profile`]) and kept with the pane's holder,
/// so it moves with it.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) enum PaneKind {
    /// Nothing yet: a pane just made, before its list or session.
    #[default]
    Empty,
    /// The new-tab list.
    List,
    /// A local terminal: the person's shell (after one of Local terminal's
    /// startup sequences or not, issue #13), or a command started with
    /// `moniker -- COMMAND` (issue #15). A session saves each as a shell in the
    /// folder the terminal last reported, never a sequence or a command;
    /// `started_in` is the folder a command or a sequence was started in, for
    /// one that never reports one (none for a plain shell, which reports its
    /// own).
    Local { started_in: Option<PathBuf> },
    /// A connection to the saved profile `id`, from the moment it is picked:
    /// while it waits for the keyring, and after its session has ended too.
    /// `name` is the profile's name then, which a session keeps only to say
    /// which profile is gone.
    Profile { id: String, name: String },
}

/// The holder of one pane's session: an empty box that fills the pane, which
/// later receives a chooser or a terminal. It also keeps the pane's
/// [`TabTitle`], text size, askpass marker and what it shows ([`PaneKind`]),
/// so they move with the pane when it or its tab is dragged elsewhere.
mod holder {
    use std::cell::{Cell, RefCell};

    use gtk::glib;
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;
    use gtk4 as gtk;

    use super::{FALLBACK_TITLE, OneTimeMarker, PaneKind, TabTitle, zoom};

    mod imp {
        use super::*;

        pub struct PaneHolder {
            pub(super) title: RefCell<TabTitle>,
            /// The session's one-time askpass marker, until its child exits.
            pub(super) marker: RefCell<Option<OneTimeMarker>>,
            /// The session's child has exited (or never started).
            pub(super) child_gone: Cell<bool>,
            /// The tab's text size: the font scale its terminal has, or will
            /// have once a session replaces the chooser.
            pub(super) scale: Cell<f64>,
            /// What the pane shows.
            pub(super) kind: RefCell<PaneKind>,
            /// The pane's session saves a transcript that is not written yet.
            pub(super) recording: Cell<bool>,
        }

        impl Default for PaneHolder {
            fn default() -> Self {
                PaneHolder {
                    title: RefCell::new(TabTitle::remote(FALLBACK_TITLE.to_owned())),
                    marker: RefCell::new(None),
                    child_gone: Cell::new(false),
                    scale: Cell::new(zoom::DEFAULT),
                    kind: RefCell::new(PaneKind::Empty),
                    recording: Cell::new(false),
                }
            }
        }

        #[glib::object_subclass]
        impl ObjectSubclass for PaneHolder {
            const NAME: &'static str = "MonikerPaneHolder";
            type Type = super::PaneHolder;
            type ParentType = gtk::Box;
        }

        impl ObjectImpl for PaneHolder {}
        impl WidgetImpl for PaneHolder {}
        impl BoxImpl for PaneHolder {}
    }

    glib::wrapper! {
        pub struct PaneHolder(ObjectSubclass<imp::PaneHolder>)
            @extends gtk::Box, gtk::Widget,
            @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
    }

    impl PaneHolder {
        /// An empty holder whose pane is titled `title`, at text size `scale`.
        pub(crate) fn new(title: TabTitle, scale: f64) -> PaneHolder {
            let holder: PaneHolder = glib::Object::new();
            holder.set_orientation(gtk::Orientation::Vertical);
            holder.set_hexpand(true);
            holder.set_vexpand(true);
            holder.imp().title.replace(title);
            holder.imp().scale.set(zoom::clamp(scale));
            holder
        }

        /// The pane's title, to read or change.
        pub(crate) fn title(&self) -> &RefCell<TabTitle> {
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

        /// The pane's text size.
        pub(crate) fn scale(&self) -> &Cell<f64> {
            &self.imp().scale
        }

        /// What the pane shows.
        pub(crate) fn kind(&self) -> &RefCell<PaneKind> {
            &self.imp().kind
        }

        /// Whether the pane's session saves a transcript: true from the moment
        /// it starts recording (`start_recording`) until its transcript is
        /// written (`Session::write_transcript_once`). Its header and tab show
        /// REC meanwhile (`crate::panes`).
        pub(crate) fn recording(&self) -> &Cell<bool> {
            &self.imp().recording
        }
    }
}

/// The holder of the focused pane of `page` (its only pane, in a tab with one),
/// if the page was made by [`new_holder_page`].
pub(crate) fn holder_of(page: &adw::TabPage) -> Option<PaneHolder> {
    panes::layout_of_page(page)?.focused_holder()
}

/// The terminal in `holder`, if its pane shows a session.
pub(crate) fn terminal_in(holder: &PaneHolder) -> Option<Terminal> {
    holder.first_child().and_downcast()
}

/// What the pane of `holder` is called now.
pub(crate) fn shown_title(holder: &PaneHolder) -> String {
    holder.title().borrow().shown().to_owned()
}

/// Apply the [`TabTitle`] of `holder` to its pane's header, and to its tab
/// (which is titled after its focused pane, and lists every pane's title in
/// its tooltip), with REC while it saves a transcript. The tooltip is markup
/// for GTK, so titles are escaped there: a name with `<b>` in it stays literal.
fn refresh_title(holder: &PaneHolder) {
    if let Some(layout) = panes::layout_of(holder) {
        panes::refresh(&layout);
    }
}

/// Rename the focused pane of `page` (its only pane, in a tab with one) to
/// `name` ([`rename_pane`]). Does nothing for a page that [`new_holder_page`]
/// did not make. (The app renames through [`ask_rename_pane`].)
#[cfg(test)]
pub fn rename_page(page: &adw::TabPage, name: &str) {
    if let Some(holder) = holder_of(page) {
        rename_pane(&holder, name);
    }
}

/// Rename the pane of `holder` to `name`: its header and, while it is the
/// focused pane, its tab show it, and it keeps it. A name with nothing in it
/// restores the automatic title.
pub(crate) fn rename_pane(holder: &PaneHolder, name: &str) {
    holder.title().borrow_mut().rename(name);
    refresh_title(holder);
}

/// Ask the person for a new name for the focused pane of `page`
/// ([`ask_rename_pane`]): what the tab is called.
pub fn ask_rename(page: &adw::TabPage) {
    if let Some(holder) = holder_of(page) {
        ask_rename_pane(&holder);
    }
}

/// Ask the person for a new name for the pane of `holder` ("Rename Tab" when it
/// is the only pane of its tab, "Rename Pane" otherwise). The field starts out
/// as the title shown now; Rename applies it with [`rename_pane`]. A name left
/// as it was changes nothing: renaming is what freezes an automatic title, and
/// that should not happen by pressing Rename without typing.
pub(crate) fn ask_rename_pane(holder: &PaneHolder) {
    let current = shown_title(holder);
    let alone = panes::layout_of(holder).is_none_or(|layout| layout.panes().len() <= 1);
    let entry = gtk::Entry::builder()
        .text(current.as_str())
        .activates_default(true)
        .build();
    let dialog = adw::AlertDialog::builder()
        .heading(if alone { "Rename Tab" } else { "Rename Pane" })
        .extra_child(&entry)
        .focus_widget(&entry)
        .default_response("rename")
        .close_response("cancel")
        .build();
    dialog.add_responses(&[("cancel", "Cancel"), ("rename", "Rename")]);
    dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
    dialog.choose(
        Some(holder),
        gio::Cancellable::NONE,
        glib::clone!(
            #[weak]
            holder,
            #[weak]
            entry,
            move |response| {
                let name = entry.text();
                if response == "rename" && name != current {
                    rename_pane(&holder, &name);
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
/// the Keyboard Shortcuts dialog lists: Ctrl+Shift+T/W/N (new tab, close pane,
/// new window), Ctrl+Page Up/Down (switching tabs, `win.previous-tab` and
/// `win.next-tab`), Ctrl+Shift+R/D (splitting), Ctrl+Shift+arrows (going to
/// another pane, in a tab with several), Ctrl+Shift+C/V (copy and paste) and
/// Ctrl+plus, Ctrl+minus and Ctrl+0 (the text size).
const TAB_VIEW_SHORTCUTS: adw::TabViewShortcuts = adw::TabViewShortcuts::NONE;

/// Set up `view` as the tabs of a window: the tab menu, the tab view's own
/// shortcuts ([`TAB_VIEW_SHORTCUTS`]), what closing a tab does, and closing the
/// window with its last tab.
///
/// * Closing a page writes the transcripts of its panes (once each), closes it,
///   and ends their sessions ([`end_session`]): each child is hung up, and its
///   one-time askpass marker is removed once it has exited.
/// * The menu's "Rename…" is the action `tab.rename`, on the page the menu was
///   opened for.
/// * When the last page goes the window closes. A page leaving for another
///   window (`page-detached`) writes nothing and keeps its askpass markers: its
///   sessions carry on over there.
/// * A tab dropped on a pane tiles there (`crate::panes::take_tab_drops`); its
///   page comes back empty and is closed (`crate::panes::page_attached`).
/// * A tab that is selected never leaves the keyboard focus in another tab
///   (`crate::panes::keep_focus_in_selected`): the focus it remembers may have
///   moved there with a pane.
pub fn setup_tab_view(view: &adw::TabView) {
    view.set_menu_model(Some(&tab_menu()));
    view.set_shortcuts(TAB_VIEW_SHORTCUTS);

    view.connect_close_page(|view, page| {
        let holders = panes::layout_of_page(page)
            .map(|layout| layout.holders())
            .unwrap_or_default();
        for terminal in holders.iter().filter_map(terminal_in) {
            write_transcript_of(&terminal);
        }
        view.close_page_finish(page, true);
        for holder in &holders {
            end_session(holder);
        }
        glib::Propagation::Stop
    });
    view.connect_page_attached(|view, page, _| panes::page_attached(view, page));
    view.connect_selected_page_notify(panes::keep_focus_in_selected);
    panes::take_tab_drops(view);

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

/// Let a double click on a tab's title in `bar` rename the tab, and let a pane
/// dropped on a tab become a tab of its own (`crate::panes::setup_tab_bar`).
///
/// The first click of the double click has already selected the tab, so it is the
/// selected page that is renamed. Clicks on a button in a tab (close) or beside
/// the tabs do nothing, so closing tabs in quick succession never asks anything.
pub fn setup_tab_bar(bar: &adw::TabBar) {
    panes::setup_tab_bar(bar);
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
/// wins over everything, else the profile's own setting. A local terminal
/// passes Local terminal's own switch (issue #13, [`default_local_save`]). The
/// person can still uncheck one session in the choices behind "›".
pub fn default_save(profile_setting: bool, save_all: bool) -> bool {
    save_all || profile_setting
}

/// [`default_save`] for a local terminal: Local terminal's own "Save
/// transcript" switch (`local`, as its file has it) or "Save transcripts of all
/// sessions" (`save_all`, as the settings file has it) (issue #13). The one
/// rule for every local tab: a pick in the new-tab list (and the box under its
/// "›"), a window's first tab and a restored local pane.
pub fn default_local_save(local: &LocalTerminal, save_all: bool) -> bool {
    default_save(local.transcript, save_all)
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
        // Written (or tried): the pane no longer says it saves one.
        show_recording(&terminal, false);
        if let Some(message) = transcript_notice(&result) {
            spec.notice.show_near(&terminal, &message);
        }
    }
}

/// Make `terminal`'s session a recording one if it has a `transcript` spec: the
/// terminal keeps unlimited scrollback and the returned [`Session`] writes its
/// text once (module docs), and its pane shows REC until then
/// ([`show_recording`]). `None` leaves the terminal as it is.
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
        show_recording(terminal, true);
        session
    })
}

/// Say whether the session in `terminal` saves a transcript (`on`) on the
/// pane whose holder shows it, and so on its header and tab
/// ([`PaneHolder::recording`], `crate::panes`). A terminal no longer in a
/// holder is left alone; one whose holder is in no tab only keeps the flag.
/// Called twice per recording session, when it starts and when its transcript
/// is written: nothing per frame.
fn show_recording(terminal: &Terminal, on: bool) {
    if let Some(holder) = terminal.parent().and_downcast::<PaneHolder>() {
        holder.recording().set(on);
        refresh_title(&holder);
    }
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

/// The holders of every pane of every tab of `view`.
fn holders_in(view: &adw::TabView) -> Vec<PaneHolder> {
    pages_of(view)
        .iter()
        .filter_map(panes::layout_of_page)
        .flat_map(|layout| layout.holders())
        .collect()
}

/// Write the transcript of every recording session shown in `view`.
/// Sessions that already wrote theirs are skipped.
pub fn write_transcripts(view: &adw::TabView) {
    let holders: Vec<gtk::Widget> = holders_in(view).into_iter().map(Cast::upcast).collect();
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
            for holder in holders_in(&view) {
                end_session(&holder);
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

/// A terminal that fills its pane, in the colors of the current light or dark
/// style and the font of the Preferences, with the Preferences' scrollback (a
/// session that saves a transcript then gets unlimited scrollback instead) and
/// its right-click menu ([`terminal_menu`]).
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
    add_menu_actions(&terminal);
    terminal.set_context_menu_model(Some(&terminal_menu()));
    theme::follow_style(&terminal);
    terminal
}

/// Names of the main window's tab actions (`win.new-tab`, `win.next-tab`,
/// `win.previous-tab`: Ctrl+Shift+T, Ctrl+Page Down and Ctrl+Page Up, and the
/// ＋ button). `window.rs` defines them; the accelerators are in `main.rs`'s
/// `SHORTCUTS`. The pane actions (Ctrl+Shift+W closes the focused pane) are
/// named in `crate::panes`.
pub(crate) const NEW_TAB_ACTION: &str = "new-tab";
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
/// [`MENU_COPY_ACTION`] and [`MENU_PASTE_ACTION`].
const TERMINAL_ACTIONS: &str = "term";

/// The terminal's own Copy (`term.copy`), the one in its right-click menu. It
/// is enabled only while that terminal has a selection, which is what makes the
/// menu entry insensitive without one. It is not the action of the
/// accelerator: `win.copy` stays enabled whenever there is a terminal, so
/// Ctrl+Shift+C never reaches the program ([`copy_selection`]).
const MENU_COPY_ACTION: &str = "copy";

/// The terminal's own Paste (`term.paste`), the one in its right-click menu: it
/// pastes into that terminal, which a right click does not give the focus, so
/// in a tab with several panes `win.paste` would paste into the focused one.
const MENU_PASTE_ACTION: &str = "paste";

/// Give `terminal` its `term.copy` and `term.paste` actions
/// ([`MENU_COPY_ACTION`], [`MENU_PASTE_ACTION`]). The terminal owns the action
/// group and the group's closures hold the terminal weakly, so nothing keeps
/// the terminal alive.
fn add_menu_actions(terminal: &Terminal) {
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
    let paste = gio::SimpleAction::new(MENU_PASTE_ACTION, None);
    paste.connect_activate(glib::clone!(
        #[weak]
        terminal,
        move |_, _| terminal.paste_clipboard()
    ));
    let actions = gio::SimpleActionGroup::new();
    actions.add_action(&copy);
    actions.add_action(&paste);
    terminal.insert_action_group(TERMINAL_ACTIONS, Some(&actions));
}

/// The menu a terminal shows on right-click: Copy (`term.copy`, insensitive
/// without a selection) and Paste (`term.paste`), then Split Right and Split
/// Down, which split this terminal's pane (`crate::panes`). VTE shows it on a
/// plain right-click, and on Shift+right-click when the program has turned on
/// mouse reporting (htop, mc, tmux with mouse on), which then gets the plain
/// right-click itself.
pub(crate) fn terminal_menu() -> gio::Menu {
    let menu = gio::Menu::new();
    menu.append(
        Some("Copy"),
        Some(&format!("{TERMINAL_ACTIONS}.{MENU_COPY_ACTION}")),
    );
    menu.append(
        Some("Paste"),
        Some(&format!("{TERMINAL_ACTIONS}.{MENU_PASTE_ACTION}")),
    );
    menu.append_section(None, &panes::split_menu_section());
    menu
}

/// The terminal of the focused pane of `view`'s selected tab, if that pane
/// shows a session.
fn selected_terminal(view: &adw::TabView) -> Option<Terminal> {
    terminal_of(&view.selected_page()?)
}

/// The holder of the focused pane of `view`'s selected tab.
pub(crate) fn selected_holder(view: &adw::TabView) -> Option<PaneHolder> {
    holder_of(&view.selected_page()?)
}

/// Copy `terminal`'s selection to the clipboard as plain text; nothing
/// happens without one.
fn copy_selection_of(terminal: &Terminal) {
    if terminal.has_selection() {
        terminal.copy_clipboard_format(Format::Text);
    }
}

/// Copy the focused pane's selection to the clipboard (the action behind
/// Ctrl+Shift+C). With no selection it does nothing, and still counts as
/// handled: the action is enabled whenever there is a terminal, because a
/// disabled one would leave the key to the terminal, where Ctrl+Shift+C may
/// act as Ctrl+C and interrupt the program.
pub(crate) fn copy_selection(view: &adw::TabView) {
    if let Some(terminal) = selected_terminal(view) {
        copy_selection_of(&terminal);
    }
}

/// Paste the clipboard into the focused pane's terminal (the action behind
/// Ctrl+Shift+V). It is VTE's own paste, as the menu's is, so a program
/// that asked for bracketed paste (shells with it do) gets a multi-line paste
/// as one block instead of running it line by line.
pub(crate) fn paste_clipboard(view: &adw::TabView) {
    if let Some(terminal) = selected_terminal(view) {
        terminal.paste_clipboard();
    }
}

/// The main window's actions that act on its tabs and panes, enabled whenever
/// no dialog is open over the window ([`sync_window_actions`]).
const TAB_WINDOW_ACTIONS: [&str; 6] = [
    NEW_TAB_ACTION,
    panes::CLOSE_PANE_ACTION,
    NEXT_TAB_ACTION,
    PREVIOUS_TAB_ACTION,
    panes::SPLIT_RIGHT_ACTION,
    panes::SPLIT_DOWN_ACTION,
];

/// The main window's actions that act among the panes of the selected tab
/// (going to another one, moving one out), enabled while no dialog is open and
/// the tab has two panes or more ([`sync_window_actions`]). Disabled, the
/// arrows' keys reach the terminal, where VTE scrolls with them
/// (Ctrl+Shift+Up/Down a line, Ctrl+Shift+Left/Right to a prompt).
const SPLIT_WINDOW_ACTIONS: [&str; 6] = [
    panes::PANE_LEFT_ACTION,
    panes::PANE_RIGHT_ACTION,
    panes::PANE_UP_ACTION,
    panes::PANE_DOWN_ACTION,
    panes::TO_TAB_ACTION,
    panes::TO_WINDOW_ACTION,
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
///   enabled with a terminal in the focused pane of the selected tab.
/// * The actions among panes (going to another, moving one out) are enabled
///   while the selected tab has two panes or more.
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
    let split = free && panes::selected_is_split(view);
    let actions = [
        (&TAB_WINDOW_ACTIONS[..], free),
        (&SPLIT_WINDOW_ACTIONS[..], split),
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

/// The text size of the focused pane of `page`, if it was made by
/// [`new_holder_page`]: the font scale its terminal has, or will have once it
/// shows a session.
pub(crate) fn scale_of(page: &adw::TabPage) -> Option<f64> {
    Some(holder_of(page)?.scale().get())
}

/// Change the text size of the focused pane of `view`'s selected tab from the
/// size it has now, within the range `crate::zoom` allows. Does nothing for a
/// pane that shows no terminal (the new-tab list): the actions are disabled
/// then.
pub(crate) fn zoom_selected(view: &adw::TabView, change: fn(f64) -> f64) {
    let Some(holder) = selected_holder(view) else {
        return;
    };
    let Some(terminal) = terminal_in(&holder) else {
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

/// A new, selected page of `view` called `title` that holds nothing yet: one
/// pane, whose holder ([`holder_of`]) a chooser or a session
/// ([`start_local_in`], [`start_profile`], [`start_command`]) later fills.
/// The tab starts at the text size of the focused pane of the tab that is
/// selected in `view` now, or at what `fallback_scale` returns if there is
/// none. (A function, so that the saved size is read from its file only for a
/// window that has no tab to take the size from, not for every tab opened.)
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
    let holder = PaneHolder::new(TabTitle::remote(title.to_owned()), scale);
    let layout = panes::new_layout(&holder);
    let page = view.append(&layout);
    view.set_selected_page(&page);
    panes::refresh(&layout);
    page
}

/// Show `chooser` in `holder`, a fresh pane that holds nothing yet: what the
/// pane shows until a session replaces it (the new-tab list).
pub(crate) fn show_chooser(holder: &PaneHolder, chooser: &impl IsA<gtk::Widget>) {
    debug_assert!(holder.first_child().is_none(), "a fresh pane");
    holder.kind().replace(PaneKind::List);
    holder.append(chooser);
}

/// The terminal of the focused pane of `page` (its only pane, in a tab with
/// one), if it holds a session.
pub fn terminal_of(page: &adw::TabPage) -> Option<Terminal> {
    terminal_in(&holder_of(page)?)
}

/// End the session shown in `holder`, whose pane (or tab, or window) has
/// closed: take
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
fn end_session(holder: &PaneHolder) {
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

/// Close the pane of `holder`, as its close button and Ctrl+Shift+W do. The
/// only pane of a tab is the tab: that closes as its close button does (every
/// transcript written, every session ended, the window closed with its last
/// tab). A pane among others writes its transcript, ends its session
/// ([`end_session`]) and leaves its room to the others (`crate::panes`).
pub(crate) fn close_pane(holder: &PaneHolder) {
    let Some(layout) = panes::layout_of(holder) else {
        return;
    };
    if layout.panes().len() <= 1 {
        if let Some((view, page)) = panes::page_of(&layout) {
            view.close_page(&page);
        }
        return;
    }
    if let Some(terminal) = terminal_in(holder) {
        write_transcript_of(&terminal);
    }
    end_session(holder);
    panes::take_out(holder);
}

/// Empty `holder` and put a new terminal in it, with the automatic title
/// `title` (a name the person gave the pane is kept). The terminal has the
/// focus if its pane is in front ([`in_front`]). Only for a pane that has not
/// run a session yet (empty, or showing a chooser): a session replaced here
/// would lose its unwritten transcript.
fn begin_session(holder: &PaneHolder, title: TabTitle) -> Terminal {
    holder.title().borrow_mut().adopt(title);
    refresh_title(holder);
    while let Some(old) = holder.first_child() {
        holder.remove(&old);
    }
    let terminal = new_terminal();
    terminal.set_font_scale(holder.scale().get());
    holder.append(&terminal);
    // Only into the pane the person is looking at: a session that starts late
    // (it waited for the keyring) must not take the keyboard from another tab
    // or pane, whose keys would then go to this host, or from a dialog. The
    // tab view gives it the focus when its tab is selected.
    if in_front(holder) {
        terminal.grab_focus();
    }
    // Connected first, so it is known before anything else hears of the exit.
    terminal.connect_child_exited(glib::clone!(
        #[weak]
        holder,
        move |_, _| holder.child_gone().set(true)
    ));
    // A pane that showed a chooser has a terminal now (Copy, Paste and the
    // text size actions apply).
    if let Some((view, _)) = panes::layout_of(holder).as_ref().and_then(panes::page_of) {
        sync_window_actions(&view);
    }
    terminal
}

/// Whether the pane of `holder` is the focused pane of the selected tab of its
/// view, in a window that shows no dialog (`crate::panes::in_front`).
fn in_front(holder: &PaneHolder) -> bool {
    panes::in_front(holder)
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
                eprintln!("{}", start_failure_log(&program, err.message()));
                if let Some(terminal) = weak.upgrade() {
                    show_note(&terminal, &start_failure_note(&program, err.message()));
                    // No child: nothing to wait for when the tab closes.
                    if let Some(holder) = terminal.parent().and_downcast::<PaneHolder>() {
                        holder.child_gone().set(true);
                    }
                    unpark(&terminal);
                }
            }
        },
    );
}

/// The shell a local terminal runs: [`shell_choice`] for Moniker's own `$SHELL`.
fn local_shell() -> String {
    shell_choice(std::env::var("SHELL").ok().as_deref())
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

/// A new tab with a local shell in the home folder, at the text size of the tab
/// selected in `view`, or at `fallback_scale` if there is none (the first tab of
/// a window). Closing it, or the shell exiting, closes the tab; the last tab
/// closing closes the window. With a `transcript` the session is saved like a
/// connection's (module docs). The application opens its tabs through
/// [`open_local`] (a window's first tab: `window::build_with`), so this is for
/// the tests.
#[cfg(test)]
pub fn open_local_shell(
    view: &adw::TabView,
    fallback_scale: f64,
    transcript: Option<TranscriptSpec>,
) -> Terminal {
    open_local(
        view,
        fallback_scale,
        &Start::home(&glib::home_dir()),
        transcript,
    )
}

/// A new tab for a local terminal as `start` says: the shell, or a command, in
/// the folder `start.dir`, named `start.title` if it has one (a name given, which
/// the program's own title never changes, as after a rename). The text size is
/// that of the tab selected in `view`, or `fallback_scale` if there is none (the
/// first tab of a window), and the session is saved when a `transcript` is given,
/// as for a connection. Everything is local: nothing here reaches a profile or
/// a password.
pub(crate) fn open_local(
    view: &adw::TabView,
    fallback_scale: f64,
    start: &Start,
    transcript: Option<TranscriptSpec>,
) -> Terminal {
    let page = new_holder_page_at(view, LOCAL_TITLE, || fallback_scale);
    let holder = holder_of(&page).expect("a new page has its pane");
    // Before the session, so the tab never shows another name, not even for the
    // moment before the program has said its own.
    if let Some(title) = &start.title {
        rename_pane(&holder, title);
    }
    match &start.run {
        Run::Shell => {
            let shell = local_shell();
            start_shell_in(&holder, &shell, &start.dir, transcript)
        }
        Run::Command { argv, hold } => {
            start_command_in(&holder, argv, &start.dir, *hold, transcript)
        }
    }
}

/// A local terminal picked plainly in `holder`'s pane, in the folder `dir` (as
/// [`start_local_shell_in`] takes it): the person's shell, after running
/// `sequence` first if one is given (one of Local terminal's startup
/// sequences, issue #13: its default for a plain pick or a restored pane, the
/// one chosen under "›" otherwise).
///
/// With no sequence, or one with nothing to run ([`local_command`] makes
/// nothing of it), this is exactly [`start_local_shell_in`]: a plain local
/// tab, unchanged. Otherwise it is [`start_sequence_in`] with the shell a plain
/// local tab runs ([`shell_choice`]) in the same folder, or in the home folder
/// when `dir` is not a folder or cannot be entered.
pub(crate) fn start_local_in(
    holder: &PaneHolder,
    dir: &Path,
    sequence: Option<&StartupSequence>,
    transcript: Option<TranscriptSpec>,
) -> Terminal {
    let Some(script) = local_command(sequence) else {
        return start_local_shell_in(holder, dir, transcript);
    };
    let shell = local_shell();
    let home = glib::home_dir();
    start_sequence_in(
        holder,
        &shell,
        enterable_or(dir, &home),
        &script,
        transcript,
    )
}

/// Run the person's shell in `holder`'s pane, in the folder `dir`, or in the
/// home folder when `dir` is not a folder (any more: a restored session's
/// folder may have gone) or is one this process may not enter (no search
/// permission), with nothing said about it. (VTE falls back to the home folder
/// by itself when it cannot enter a folder; this does not leave the rule to
/// it.) The pane is called after what the shell says its title is, and closes
/// when the shell exits. With a `transcript` its text is saved when the shell
/// exits or the pane, tab or window closes.
pub(crate) fn start_local_shell_in(
    holder: &PaneHolder,
    dir: &Path,
    transcript: Option<TranscriptSpec>,
) -> Terminal {
    let shell = local_shell();
    let home = glib::home_dir();
    start_shell_in(holder, &shell, enterable_or(dir, &home), transcript)
}

/// `dir` if a local terminal can start in it (a folder, named in text, that
/// this process may enter), else `home`.
fn enterable_or<'a>(dir: &'a Path, home: &'a Path) -> &'a Path {
    if folder_to_spawn_in(dir).is_some() && can_enter(dir) {
        dir
    } else {
        home
    }
}

/// A new tab in `view` for Local terminal picked anywhere but the new-tab list
/// (the Connection Profiles window's Local terminal entry, issue #13): in the
/// home folder, running `sequence` first if one is given ([`start_local_in`]),
/// and saving a transcript if `record` ([`local_transcript_spec`]). The tab
/// starts at the text size of the tab selected in `view`, or, in a window with
/// none, at the size saved for the next launch, as [`open_profile`]'s does.
pub(crate) fn open_local_pick(
    view: &adw::TabView,
    ctx: &Rc<AppContext>,
    sequence: Option<&StartupSequence>,
    record: bool,
) -> Terminal {
    let page = new_holder_page_at(view, LOCAL_TITLE, || ctx.saved_font_scale());
    let holder = holder_of(&page).expect("a new page has its pane");
    let transcript = local_transcript_spec(ctx, record);
    start_local_in(&holder, &glib::home_dir(), sequence, transcript)
}

/// Whether this process may enter the folder `dir`, as a shell started in it
/// must (search permission, `access(X_OK)`).
fn can_enter(dir: &Path) -> bool {
    rustix::fs::access(dir, rustix::fs::Access::EXEC_OK).is_ok()
}

/// The folder the program in `terminal` last reported as its own (the shell's
/// OSC 7, VTE's current directory: `file://HOST/PATH`, percent-encoded), if it
/// did ([`folder_of_uri`]). Read when a session is saved, never followed.
pub(crate) fn current_folder(terminal: &Terminal) -> Option<PathBuf> {
    folder_of_uri(&terminal.current_directory_uri()?, &local_host_name())
}

/// The folder that the OSC 7 address `uri` names, an absolute path, if the
/// address is one on this machine: a host that is empty, `localhost` or
/// `node_name` (this machine's name as `uname` reports it, which names local
/// transcripts too: [`local_host_name`]), in any case. Any other host is none:
/// a shell over `ssh` run by hand in a local pane reports the remote machine's
/// folders, and a program can print the sequence, and neither may choose the
/// folder a restored local shell starts in.
fn folder_of_uri(uri: &str, node_name: &str) -> Option<PathBuf> {
    let (path, host) = glib::filename_from_uri(uri).ok()?;
    let here = host.as_deref().is_none_or(|host| {
        let host = host.to_lowercase();
        host.is_empty() || host == "localhost" || host == node_name.to_lowercase()
    });
    (here && path.is_absolute()).then_some(path)
}

/// [`start_local_shell_in`] with the shell and the directory given (the tests
/// run a stand-in shell). A directory that is not there leaves the shell in the
/// directory Moniker was started in.
pub(crate) fn start_shell_in(
    holder: &PaneHolder,
    shell: &str,
    dir: &Path,
    transcript: Option<TranscriptSpec>,
) -> Terminal {
    let terminal = begin_session(holder, TabTitle::local());
    // The shell reports its own folder.
    holder.kind().replace(PaneKind::Local { started_in: None });
    // A shell that exits closes its pane, and closing a pane or a tab writes
    // the transcript ([`close_pane`], `setup_tab_view`), so there is nothing
    // more to do when it exits.
    start_recording(&terminal, transcript);
    terminal.connect_child_exited(glib::clone!(
        #[weak]
        holder,
        move |_, _status| close_pane(&holder)
    ));
    follow_program_title(holder, &terminal);
    spawn(&terminal, folder_to_spawn_in(dir), &[shell.to_owned()], &[]);
    terminal
}

/// Run the command `argv` (the program first, then its arguments, as a list:
/// nothing here builds a shell string) in `holder`'s pane, in the folder `dir`
/// (as [`start_shell_in`] takes it). The pane is named after the program's file
/// name and follows the title the program sets ([`TabTitle::program`]). The
/// environment is the inherited one, as a local shell's is: Moniker's own,
/// which is its launcher's if this launch started Moniker. A launch handed to a
/// running Moniker brings none.
///
/// When the command ends: a clean exit closes the pane (and the window with its
/// last one), any other end keeps it with the note a connection shows
/// ([`exit_action`]); with `hold` every end keeps it with [`hold_note`],
/// success included. A transcript is saved when the command ends or the pane,
/// tab or window closes, whichever is first, and holds no note.
///
/// A command that cannot be started (no program, an empty name or one that is
/// not there) says so in the pane, "[could not start …]", which stays.
pub(crate) fn start_command_in(
    holder: &PaneHolder,
    argv: &[String],
    dir: &Path,
    hold: bool,
    transcript: Option<TranscriptSpec>,
) -> Terminal {
    let program = argv.first().map(String::as_str).unwrap_or_default();
    let terminal = begin_session(holder, TabTitle::program(program));
    // A local terminal for a saved session, in the folder the command really
    // started in; never its command.
    holder.kind().replace(PaneKind::Local {
        started_in: started_in(dir),
    });
    let session = start_recording(&terminal, transcript);
    end_as_a_command(holder, &terminal, session, hold);
    follow_program_title(holder, &terminal);
    if program.is_empty() {
        // Nothing to start: say so as a failed start does, without asking VTE
        // or GLib to run a program with no name. There is no child to wait for
        // when the pane closes.
        show_note(&terminal, "[could not start: no program to run]");
        holder.child_gone().set(true);
    } else {
        spawn(&terminal, folder_to_spawn_in(dir), argv, &[]);
    }
    terminal
}

/// Run the startup sequence `script` (what [`local_command`] makes of one of
/// Local terminal's sequences, issue #13) in `holder`'s pane with the shell
/// `shell`, in the folder `dir` (as [`start_shell_in`] takes it): the argument
/// list `[shell, "-c", script]`, with `SHELL=<shell>` added to the inherited
/// environment, so the script's last line, `exec "$SHELL"`, leaves that same
/// shell, interactive, even when Moniker has no `$SHELL`. The shell's path
/// travels in the environment only, never in the script, so a path with a space
/// or a quote cannot change what runs. The commands are the person's own, in
/// their own shell's language, as a profile's are in the remote login shell's.
///
/// The pane is named as a plain local tab is ([`TabTitle::local`], following
/// the shell's title until renamed) and saves a transcript as one does. A saved
/// session keeps it as a local pane (never its commands) in the folder its
/// shell last reports, else in the folder it started in, as a command tab is
/// kept: the shell running the sequence reports none, so a pane saved while a
/// long command of the sequence runs (`journalctl -f`, `htop`) keeps its
/// folder, and once the interactive shell reports its own, that one counts.
/// It ends as a command tab does
/// ([`start_command_in`] without `hold`), not as a plain shell: a clean exit
/// closes the pane, any other end keeps it with the note ([`exit_action`]).
/// Ctrl+C during a long command of the sequence ends the non-interactive shell
/// running it too, and a plain shell's rule would close the pane and take the
/// output with it. The cost: `exit 3` in the shell it leaves keeps the pane. A
/// transcript is written when the shell ends or the pane, tab or window closes,
/// whichever is first.
pub(crate) fn start_sequence_in(
    holder: &PaneHolder,
    shell: &str,
    dir: &Path,
    script: &str,
    transcript: Option<TranscriptSpec>,
) -> Terminal {
    let terminal = begin_session(holder, TabTitle::local());
    // The folder it really starts in, as for a command tab, until the shell it
    // leaves reports its own.
    holder.kind().replace(PaneKind::Local {
        started_in: started_in(dir),
    });
    let session = start_recording(&terminal, transcript);
    end_as_a_command(holder, &terminal, session, false);
    follow_program_title(holder, &terminal);
    let argv = [shell.to_owned(), "-c".to_owned(), script.to_owned()];
    spawn(
        &terminal,
        folder_to_spawn_in(dir),
        &argv,
        &[format!("SHELL={shell}")],
    );
    terminal
}

/// When the child in `terminal` (`holder`'s pane) exits, end as a command tab
/// does: write the transcript of `session`, if it records, then close the pane
/// or keep it with the note, as [`command_exit_action`] says for `hold`.
fn end_as_a_command(
    holder: &PaneHolder,
    terminal: &Terminal,
    session: Option<Rc<Session>>,
    hold: bool,
) {
    terminal.connect_child_exited(glib::clone!(
        #[weak]
        holder,
        move |terminal, status| {
            if let Some(session) = &session {
                session.write_transcript_once();
            }
            match command_exit_action(status, hold) {
                ExitAction::Close => close_pane(&holder),
                ExitAction::Keep { note } => show_note(terminal, &note),
            }
        }
    ));
}

/// Let the title the program in `terminal` sets name `holder`'s pane, until the
/// pane is renamed. Only a local pane (a shell or a command of the person's own)
/// gets here: what a remote host says about itself is not to be believed. (VTE
/// 0.78 deprecated `window-title` for the `xterm.title` termprop, but the
/// termprop API needs vte4's `v0_78` feature, which would raise the minimum VTE
/// from 0.76; the property is still kept up to date, as the tests check.)
fn follow_program_title(holder: &PaneHolder, terminal: &Terminal) {
    terminal.connect_window_title_notify(glib::clone!(
        #[weak]
        holder,
        move |terminal| {
            let raw = terminal.window_title().unwrap_or_default();
            holder.title().borrow_mut().set_terminal_title(&raw);
            refresh_title(&holder);
        }
    ));
}

/// The folder a program started in `dir` really starts in, as a saved session
/// records it ([`PaneKind::Local`]): `dir` if a program starts there
/// ([`folder_to_spawn_in`]), else none (it starts in the folder Moniker was
/// started in, which is not recorded).
fn started_in(dir: &Path) -> Option<PathBuf> {
    folder_to_spawn_in(dir)
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
}

/// `dir` as the folder to start a program in: only one that is a folder (and
/// text), else the program starts in the folder Moniker was started in.
fn folder_to_spawn_in(dir: &Path) -> Option<&str> {
    dir.is_dir().then(|| dir.to_str()).flatten()
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

/// Work out the pane for connecting to `profile` with its startup sequence
/// number `sequence`: its title, the `ssh` command and the transcript settings.
/// `record` says whether the session is recorded (see [`transcript_spec`]), and
/// `has_password` whether a password is stored for it (the keyring worker's
/// answer, [`has_stored_password`]). Shows a note as a notification if the
/// stored password cannot be used, in the window of `holder`, the pane being
/// opened: a connection started from the Connection Profiles window opens its
/// tab in a main window, and that is where the note belongs.
pub(crate) fn launch(
    holder: &PaneHolder,
    profile: &Profile,
    sequence: Option<usize>,
    record: bool,
    ctx: &AppContext,
    transcript_dir: &Path,
    has_password: bool,
) -> Launch {
    if let Some(note) = askpass_notice(has_password, &ctx.askpass_path) {
        ctx.notice.show_near(holder, note);
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
    ctx: &Rc<AppContext>,
    profile: &Profile,
    sequence: Option<usize>,
    record: bool,
) {
    let page = new_holder_page_at(view, &tab_title(profile), || ctx.saved_font_scale());
    let holder = holder_of(&page).expect("a new page has its pane");
    start_profile(&holder, ctx, profile, sequence, record);
}

/// [`open_profile`] in the pane of `holder`, which has not run a session yet.
///
/// Whether a password is stored is asked on the keyring worker
/// (`crate::keyring::read`), never on the main thread: a locked keyring waits
/// for its unlock prompt, and the window must not freeze meanwhile. Until the
/// answer is back the pane waits ([`show_waiting`]); then the session starts,
/// unless the pane was closed meanwhile (its tab or window too), which starts
/// nothing. A pane or tab dragged elsewhere meanwhile starts there. A keyring
/// worker that stopped counts as no stored password: ssh asks in the terminal.
///
/// The session uses the profile as the window holds it when the answer is
/// back, not as it was when it was picked: saves asked for before (a host or
/// port changed just before connecting) run first on the same worker, and the
/// window has read the file again by then (issue #7). The startup sequence is
/// the one the person picked, as they saw it.
pub(crate) fn start_profile(
    holder: &PaneHolder,
    ctx: &Rc<AppContext>,
    profile: &Profile,
    sequence: Option<usize>,
    record: bool,
) {
    let waiting = show_waiting(holder);
    // The profile's pane from now on, also while it waits and after its
    // session has ended.
    holder.kind().replace(PaneKind::Profile {
        id: profile.id.clone(),
        name: profile.name.clone(),
    });
    // Named after the profile at once, while it waits (a name the person gave
    // the pane is kept).
    holder
        .title()
        .borrow_mut()
        .adopt(TabTitle::remote(tab_title(profile)));
    refresh_title(holder);
    let asked = profile.clone();
    let profile = profile.clone();
    let holder = holder.downgrade();
    let ctx_done = Rc::clone(ctx);
    crate::keyring::read(
        ctx,
        move |secrets| has_stored_password(&asked, secrets),
        move |has_password| {
            let ctx = ctx_done;
            let Some(holder) = holder.upgrade() else {
                return;
            };
            // Closing the pane, its tab or its window emptied the holder.
            let still_waiting = holder.first_child().is_some_and(|child| child == waiting);
            if !still_waiting {
                return;
            }
            // Bind first so the store borrow ends here.
            let now = ctx.store.borrow().get(&profile.id).cloned();
            let profile = match now {
                Some(mut now) => {
                    now.sequences = profile.sequences;
                    now
                }
                None => profile,
            };
            let Launch {
                title,
                command,
                transcript,
                marker,
            } = launch(
                &holder,
                &profile,
                sequence,
                record,
                &ctx,
                &ctx.transcripts_dir(),
                has_password.unwrap_or(false),
            );
            start_command(
                &holder,
                &title,
                test_hook::command(command),
                transcript,
                marker,
            );
        },
    );
}

/// How long a tab waits for the keyring before it says so: a quick answer
/// shows nothing.
const WAITING_SHOWN_AFTER: Duration = Duration::from_millis(400);

/// What the tab says while it waits for the keyring.
pub(crate) const WAITING_TEXT: &str = "Waiting for the keyring\u{2026}";

/// Show in `holder` (which has not run a session yet) that it waits for the
/// keyring before its session starts, in place of whatever it showed (the
/// new-tab list, so that a second pick there starts nothing), and return
/// what it shows now. The spinner and its line appear only after
/// [`WAITING_SHOWN_AFTER`].
fn show_waiting(holder: &PaneHolder) -> gtk::Widget {
    while let Some(old) = holder.first_child() {
        holder.remove(&old);
    }
    let waiting = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .hexpand(true)
        .vexpand(true)
        .build();
    let spinner = adw::Spinner::builder()
        .width_request(32)
        .height_request(32)
        .build();
    let label = gtk::Label::builder()
        .label(WAITING_TEXT)
        .css_classes(["dim-label"])
        .build();
    spinner.set_visible(false);
    label.set_visible(false);
    waiting.append(&spinner);
    waiting.append(&label);
    holder.append(&waiting);
    glib::timeout_add_local_once(
        WAITING_SHOWN_AFTER,
        glib::clone!(
            #[weak]
            spinner,
            #[weak]
            label,
            move || {
                spinner.set_visible(true);
                label.set_visible(true);
            }
        ),
    );
    waiting.upcast()
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
    let holder = holder_of(&page).expect("a new page has its pane");
    start_command(&holder, title, cmd, transcript, None)
}

/// [`open_command`] in the pane of `holder`, which has not run a session yet.
/// The pane is called `title` and never takes another from the program it
/// runs.
///
/// `marker` is the connection's one-time askpass marker: the pane keeps it and
/// removes it when the child exits or the pane, tab or window closes (module
/// docs).
pub(crate) fn start_command(
    holder: &PaneHolder,
    title: &str,
    cmd: SshCommand,
    transcript: Option<TranscriptSpec>,
    marker: Option<OneTimeMarker>,
) -> Terminal {
    let terminal = begin_session(holder, TabTitle::remote(title.to_owned()));
    holder.marker().replace(marker);
    let session = start_recording(&terminal, transcript);

    terminal.connect_child_exited(glib::clone!(
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
                ExitAction::Close => close_pane(&holder),
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

    // hold_note and the end of a command tab

    #[test]
    fn hold_note_names_every_end() {
        assert_eq!(hold_note(0), "[session ended: status 0]");
        assert_eq!(hold_note(1 << 8), "[session ended: status 1]");
        assert_eq!(hold_note(255 << 8), "[session ended: status 255]");
        assert_eq!(hold_note(9), "[session ended: signal 9]");
        assert_eq!(hold_note(0x80 | 11), "[session ended: signal 11]");
        assert_eq!(hold_note(0x137f), "[session ended: status 4991]");
    }

    #[test]
    fn hold_note_and_exit_action_write_the_same_text_for_every_nonzero_end() {
        // Every exit code, every signal (with and without the core-dumped
        // bit) and the stopped status: what a connection shows and what a held
        // command shows must never drift apart.
        let statuses = (1..=255)
            .map(|code| code << 8)
            .chain((1..=64).flat_map(|signal| [signal, signal | 0x80]))
            .chain([0x137f, 0x7f]);
        for status in statuses {
            assert_eq!(
                exit_action(status),
                keep(&hold_note(status)),
                "status {status:#x}"
            );
        }
    }

    #[test]
    fn a_command_tab_closes_on_success_and_keeps_every_other_end() {
        assert_eq!(command_exit_action(0, false), ExitAction::Close);
        for status in [1 << 8, 3 << 8, 9, 0x80 | 11, 0x137f] {
            assert_eq!(command_exit_action(status, false), keep(&hold_note(status)));
        }
    }

    #[test]
    fn a_held_command_tab_keeps_every_end_with_the_note() {
        assert_eq!(
            command_exit_action(0, true),
            keep("[session ended: status 0]")
        );
        for status in [1 << 8, 3 << 8, 9, 0x80 | 11, 0x137f] {
            assert_eq!(
                command_exit_action(status, true),
                command_exit_action(status, false),
                "a failure is kept either way, with the same note"
            );
        }
    }

    // A command's name

    #[test]
    fn a_command_tab_is_named_after_its_program() {
        assert_eq!(program_title("htop"), "htop");
        assert_eq!(program_title("/usr/bin/htop"), "htop");
        assert_eq!(program_title("/opt/My App/run me"), "run me");
        assert_eq!(program_title("./tool/"), "tool");
        // Nothing to take a file name from: the text as given, cleaned.
        assert_eq!(program_title("/"), "/");
        assert_eq!(program_title(".."), "..");
        // Never a blank or control characters in a title.
        assert_eq!(program_title("/bin/a\u{1b}b"), "a b");
        assert_eq!(program_title(""), "Local terminal");
        assert_eq!(program_title("\n"), "Local terminal");
    }

    #[test]
    fn a_command_tab_follows_its_program_until_renamed() {
        let mut t = TabTitle::program("htop");
        assert_eq!(t.shown(), "htop");
        t.set_terminal_title("htop - desktop");
        assert_eq!(t.shown(), "htop - desktop");
        // A program that clears its title gets its own name back, not the
        // shell's.
        t.set_terminal_title("");
        assert_eq!(t.shown(), "htop");
        // A given title stays whatever the program says.
        t.rename("Logs");
        t.set_terminal_title("x");
        assert_eq!(t.shown(), "Logs");
        t.rename("");
        assert_eq!(t.shown(), "x");
    }

    // The note of a program that did not start

    #[test]
    fn a_start_failure_is_one_line_of_plain_text() {
        assert_eq!(
            start_failure_note("htop", "No such file"),
            "[could not start htop: No such file]"
        );
        // Neither the program (any `-e` argument) nor the reason can act on
        // the terminal that is fed the note: a title, a clear and a line
        // break come out as spaces.
        assert_eq!(
            start_failure_note("\x1b]0;pwned\x07\nx", "a\x1b[2Jb\r\nc"),
            "[could not start  ]0;pwned  x: a [2Jb  c]"
        );
        // What is not a control character stays as it is.
        assert_eq!(
            start_failure_note("ünï <b> & 'x'", "r"),
            "[could not start ünï <b> & 'x': r]"
        );
    }

    #[test]
    fn the_start_failure_on_standard_error_is_plain_text_too() {
        assert_eq!(
            start_failure_log("htop", "No such file"),
            "moniker: failed to start htop: No such file"
        );
        assert_eq!(
            start_failure_log("\x1b]0;pwned\x07\nx", "a\x1b[2Jb\r\nc"),
            "moniker: failed to start  ]0;pwned  x: a [2Jb  c"
        );
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

    /// What a saved session keeps as a pane's name: only one the person gave,
    /// cleaned, and never a name of nothing.
    #[test]
    fn only_a_name_given_is_the_renamed_one() {
        let mut t = TabTitle::local();
        assert_eq!(t.renamed(), None);
        t.set_terminal_title("vim");
        assert_eq!(t.renamed(), None, "a program's title is not a name given");
        t.rename("  Logs\n");
        assert_eq!(t.renamed(), Some("Logs"));
        t.rename(" \t ");
        assert_eq!(t.renamed(), None, "a name of nothing is no name");
        t.rename("x");
        t.rename("");
        assert_eq!(t.renamed(), None);
        let mut t = TabTitle::remote("web".into());
        t.rename("Kept");
        t.adopt(TabTitle::remote("web, renamed".into()));
        assert_eq!(t.renamed(), Some("Kept"), "kept when the session starts");
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
    fn a_folder_is_taken_only_from_this_machine() {
        let folder = |uri: &str, node: &str| folder_of_uri(uri, node);
        let tmp = Some(PathBuf::from("/tmp/My Files/Ü"));
        let encoded = "/tmp/My%20Files/%C3%9C";
        // No host, localhost, or this machine's node name, in any case.
        for host in ["", "localhost", "LocalHost", "box", "BOX", "Box"] {
            assert_eq!(
                folder(&format!("file://{host}{encoded}"), "box"),
                tmp,
                "{host:?}"
            );
        }
        assert_eq!(
            folder(&format!("file://box{encoded}"), "BOX"),
            tmp,
            "a node name in capitals"
        );
        // Any other host is not this machine: a shell over ssh run by hand in
        // a local pane, or a printed escape sequence, reports one.
        for host in [
            "other",
            "box.example.org",
            "boxes",
            "bo",
            "127.0.0.1",
            "[::1]",
        ] {
            assert_eq!(
                folder(&format!("file://{host}{encoded}"), "box"),
                None,
                "{host:?}"
            );
        }
        // Not a file address, or not one at all.
        for uri in ["http://box/tmp", "/tmp", "", "file:tmp"] {
            assert_eq!(folder(uri, "box"), None, "{uri:?}");
        }
    }

    #[test]
    fn a_folder_that_cannot_be_entered_is_not_entered() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        assert!(can_enter(dir.path()));
        assert!(!can_enter(&dir.path().join("gone")));
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        // Search permission is what counts: read permission alone is not.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o100)).unwrap();
        assert!(can_enter(&locked), "search only");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o600)).unwrap();
        let root = rustix::process::geteuid().is_root();
        assert_eq!(can_enter(&locked), root, "read and write, no search");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert_eq!(can_enter(&locked), root, "nothing");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
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
