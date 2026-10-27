//! The Local terminal page (issue #13): the content pane of the Connection
//! Profiles window while the sidebar's Local terminal entry is selected.
//!
//! Two groups. "Startup sequences" lists Local terminal's sequences, each
//! opening its sub-page ([`sequence_page`], for [`Owner::Local`]), then, while
//! there is a sequence, the "Default sequence" row, then a row that adds one:
//! the rows a profile's page has ([`SequenceRows`]), with their own words
//! ([`LOCAL_WORDING`]). The component owns that group, so the group holds
//! nothing else. "Session" holds Local terminal's own "Save transcript"
//! switch, with a profile's subtitle.
//!
//! All of it is kept in the GUI settings file, this machine's own and never
//! synced ([`local_terminal`]), never in the profile file. The settings file
//! takes no lock and its writes are quick, so the saves here run on the main
//! thread, at once, as the Preferences' do; a profile's go to the keyring
//! worker (issue #7) because a sync can hold the profile file. A save that
//! worked tells every list and window ([`AppContext::profiles_changed`]), as a
//! profile's save does. One that did not (a settings file that cannot be read
//! is never written over; a pick of the default is refused if the list
//! changed since the page showed it) is told near its row, which is marked
//! `error` and shows the file again. After a pick of the default, both wait
//! for the main loop, since the pick is told from inside the row's own signal
//! ([`Inner::pick`]).
//!
//! The page shows the settings file as it is when it is made and at every
//! [`LocalPage::refresh`] (the window calls it after every change). A file
//! that cannot be read reads as the defaults: no sequence, the switch off.
//!
//! Sequence names are shown as plain text (markup off) although they are this
//! machine's own: the file can be edited by hand.

use std::cell::Cell;
use std::rc::{Rc, Weak};

use adw::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;

use super::page::TRANSCRIPT_WARNING;
use super::sequence_page::{self, Owner};
use super::sequence_rows::{Choice, LOCAL_WORDING, SequenceRows};
use crate::context::AppContext;
use crate::local_terminal::{self, LOCAL_SEQUENCES_NOTE, LocalTerminal};

/// Local terminal's page. Dropping it drops its state; the widgets go with the
/// page widget.
pub(super) struct LocalPage {
    pub widget: adw::PreferencesPage,
    inner: Rc<Inner>,
}

struct Inner {
    /// Itself, for what runs after a pick: it holds the page weakly.
    me: Weak<Inner>,
    ctx: Rc<AppContext>,
    /// The "Startup sequences" group: where the sub-page opens from, and what
    /// `rows` fills.
    sequences: adw::PreferencesGroup,
    rows: Rc<SequenceRows>,
    transcript: adw::SwitchRow,
    /// What the file says about the switch: showing it is not a change.
    transcript_stored: Cell<bool>,
}

impl LocalPage {
    /// The page, showing `local` (Local terminal as the settings file has it).
    pub(super) fn new(ctx: &Rc<AppContext>, local: &LocalTerminal) -> Rc<LocalPage> {
        let widget = adw::PreferencesPage::new();
        let sequences = adw::PreferencesGroup::builder()
            .title("Startup sequences")
            .description(LOCAL_SEQUENCES_NOTE)
            .build();
        widget.add(&sequences);
        let transcript = adw::SwitchRow::builder()
            .title("Save transcript")
            .subtitle(TRANSCRIPT_WARNING)
            .use_markup(false)
            .active(local.transcript)
            .build();
        let session = adw::PreferencesGroup::builder().title("Session").build();
        session.add(&transcript);
        widget.add(&session);

        let inner = Rc::new_cyclic(|weak: &Weak<Inner>| Inner {
            me: weak.clone(),
            ctx: Rc::clone(ctx),
            sequences: sequences.clone(),
            rows: SequenceRows::new(
                &sequences,
                LOCAL_WORDING,
                {
                    let weak = weak.clone();
                    move |index| {
                        if let Some(inner) = weak.upgrade() {
                            inner.open_sequence(index);
                        }
                    }
                },
                {
                    let weak = weak.clone();
                    move |choice| {
                        if let Some(inner) = weak.upgrade() {
                            inner.pick(choice);
                        }
                    }
                },
            ),
            transcript,
            transcript_stored: Cell::new(local.transcript),
        });
        inner.show(local);
        let weak = Rc::downgrade(&inner);
        inner.transcript.connect_active_notify(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.transcript_changed();
            }
        });
        Rc::new(LocalPage { widget, inner })
    }

    /// Show `local` (as the settings file has it now) in every row. The rows
    /// of the sequences are made again only if what they show has changed.
    pub(super) fn refresh(&self, local: &LocalTerminal) {
        self.inner.show(local);
    }
}

impl Inner {
    /// Show `local` in every row (see [`LocalPage::refresh`]). Setting the
    /// switch to what the file says is not a change, and saves nothing.
    fn show(&self, local: &LocalTerminal) {
        self.transcript_stored.set(local.transcript);
        if self.transcript.is_active() != local.transcript {
            self.transcript.set_active(local.transcript);
        }
        self.rows.show(&local.sequences, local.startup_choice());
    }

    /// Show the settings file again, after a save it refused.
    fn show_file(&self) {
        self.show(&self.ctx.settings().local_terminal);
    }

    /// Open the sub-page for the sequence at `index` (`None`: a new one) over
    /// this page. If it cannot be read (the file changed under the rows on
    /// show) the person is told and the rows are brought up to date.
    fn open_sequence(&self, index: Option<usize>) {
        let nav = self
            .sequences
            .ancestor(adw::NavigationView::static_type())
            .and_downcast::<adw::NavigationView>();
        let Some(nav) = nav else {
            return;
        };
        if let Err(message) = sequence_page::push(&nav, &self.ctx, &Owner::Local, index) {
            self.ctx.notice.show_near(&self.sequences, &message);
            self.ctx.profiles_changed();
        }
    }

    /// The person picked the default sequence in its row: save it, at once,
    /// unless the file has another sequence at that place by now
    /// ([`local_terminal::set_default`]).
    ///
    /// What the page then shows (every list told of a save that worked, the
    /// file shown again after one that did not) waits for the main loop: this
    /// runs inside the row's own signal, and showing the file can make the
    /// row's list again, which the row does not survive while it is still
    /// telling of the pick (it crashes). Until then the row shows the pick, as
    /// a profile's does while its save is out.
    fn pick(&self, choice: Choice) {
        let row = self.rows.default_row();
        let saved = local_terminal::set_default(&self.ctx.settings_path, choice);
        match &saved {
            Ok(()) => row.remove_css_class("error"),
            Err(message) => {
                row.add_css_class("error");
                self.ctx.notice.show_near(row, message);
            }
        }
        let ctx = Rc::clone(&self.ctx);
        let me = self.me.clone();
        glib::idle_add_local_once(move || match saved {
            Ok(()) => ctx.profiles_changed(),
            Err(_) => {
                if let Some(inner) = me.upgrade() {
                    inner.show_file();
                }
            }
        });
    }

    /// The switch changed: save it. If that fails it goes back to what the
    /// file says (a switch that shows an unsaved state would mislead).
    fn transcript_changed(&self) {
        let on = self.transcript.is_active();
        if on == self.transcript_stored.get() {
            return;
        }
        match local_terminal::set_transcript(&self.ctx.settings_path, on) {
            Ok(()) => {
                self.transcript_stored.set(on);
                self.transcript.remove_css_class("error");
                self.ctx.profiles_changed();
            }
            Err(message) => {
                self.transcript.add_css_class("error");
                self.ctx.notice.show_near(&self.transcript, &message);
                self.show_file();
            }
        }
    }
}
