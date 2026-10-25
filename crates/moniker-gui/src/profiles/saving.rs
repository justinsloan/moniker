//! What a row of the profile page (or the startup-sequence sub-page) shows
//! while its saves are not back from the keyring worker yet (issue #7): the
//! profile file can be held by a sync that waits for the keyring, so a save can
//! take as long as the person takes to answer an unlock prompt.
//!
//! [`Saving`] counts the saves under way. While there is one, the page leaves
//! what the row shows alone (a refresh does not put the file's value back),
//! and after [`SHOWN_AFTER`] a spinner at the row's end says it waits. A save
//! that is back in time shows nothing, so the usual quick save does not
//! flicker.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

/// How long a save may take before its spinner shows (as connecting waits for
/// the keyring: `terminal_tab`).
const SHOWN_AFTER: Duration = Duration::from_millis(400);

/// The saves of one row under way, and its spinner (module docs).
pub(super) struct Saving {
    pending: Cell<usize>,
    /// Hidden until a save has waited [`SHOWN_AFTER`]; the owner puts it at
    /// the end of its row.
    pub(super) spinner: adw::Spinner,
    /// Shows the spinner once the first save has waited long enough.
    cue: RefCell<Option<glib::SourceId>>,
}

impl Saving {
    pub(super) fn new() -> Rc<Saving> {
        let spinner = adw::Spinner::builder()
            .valign(gtk::Align::Center)
            .tooltip_text("Saving\u{2026}")
            .visible(false)
            .build();
        spinner
            .upcast_ref::<gtk::Widget>()
            .update_property(&[gtk::accessible::Property::Label("Saving\u{2026}")]);
        Rc::new(Saving {
            pending: Cell::new(0),
            spinner,
            cue: RefCell::new(None),
        })
    }

    /// Whether a save is under way.
    pub(super) fn busy(&self) -> bool {
        self.pending.get() > 0
    }

    /// A save is asked for.
    pub(super) fn begin(self: &Rc<Self>) {
        self.pending.set(self.pending.get() + 1);
        if self.cue.borrow().is_some() || self.spinner.is_visible() {
            return;
        }
        let weak = Rc::downgrade(self);
        let cue = glib::timeout_add_local_once(SHOWN_AFTER, move || {
            if let Some(saving) = weak.upgrade() {
                // It fired: forget it, so nothing removes it.
                saving.cue.take();
                saving.spinner.set_visible(saving.busy());
            }
        });
        self.cue.replace(Some(cue));
    }

    /// A save is back.
    pub(super) fn end(&self) {
        self.pending.set(self.pending.get().saturating_sub(1));
        if self.busy() {
            return;
        }
        if let Some(cue) = self.cue.take() {
            cue.remove();
        }
        self.spinner.set_visible(false);
    }
}
