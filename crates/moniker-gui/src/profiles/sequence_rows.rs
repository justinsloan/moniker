//! The startup-sequence rows of a page, and its "Default sequence" row: one
//! component for the profile page and (issue #13) the Local terminal page, so
//! that the two list and choose sequences the same way.
//!
//! [`SequenceRows`] puts into a group one row per sequence (its name and number
//! of commands), each opening the sequence's sub-page, or a note that there are
//! none; the "Default sequence" combo row, while there is a sequence to choose;
//! and a row that adds a sequence. The owner passes the sequences and the
//! default it has read ([`SequenceRows::show`]) whenever it refreshes: the rows
//! are made again only when what they show (each name and its number of
//! commands) has changed, so a refresh that brings nothing new leaves the rows,
//! and the keyboard on them, alone.
//!
//! The default row's list is "None" and then the sequences by name, made again
//! whenever the names change. Only a pick by the person is passed on
//! ([`SequenceRows::new`]'s `on_pick`); showing what the file says does not
//! count as one. The owner saves it, with the row and its [`Saving`] this
//! component gives it: while a pick is being saved the row keeps showing it,
//! and once it is back the next [`show`](SequenceRows::show) shows the file's
//! choice, which puts a failed pick back.
//!
//! Sequence names are data from other machines: plain text everywhere (Pango
//! markup off, and the sync page's plain-text factory for the combo row).

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;
use moniker_core::profile::StartupSequence;

use super::saving::Saving;
use super::sync_page::plain_text_factory;

/// The first entry of the "Default sequence" row: no default.
const DEFAULT_NONE: &str = "None";

/// A default picked: the sequence's place in the list together with the
/// sequence the person saw there, or `None` for "None".
pub(super) type Choice = Option<(usize, StartupSequence)>;

/// The words that differ between the pages that use the rows.
#[derive(Clone, Copy)]
pub(super) struct Wording {
    /// The dimmed note shown when there is no sequence.
    pub empty: &'static str,
    /// The "Default sequence" row's subtitle.
    pub default_subtitle: &'static str,
}

/// The profile page's words.
pub(super) const PROFILE_WORDING: Wording = Wording {
    empty: "None: connecting gives a plain shell",
    default_subtitle: "Runs every time this profile connects",
};

/// The Local terminal page's words (issue #13).
pub(super) const LOCAL_WORDING: Wording = Wording {
    empty: "None: opening gives a plain shell",
    default_subtitle: "Runs every time Local terminal is picked or restored",
};

/// The rows of one group (module docs).
pub(super) struct SequenceRows {
    /// Itself, for the handlers of the rows it makes: they hold it weakly.
    me: Weak<SequenceRows>,
    group: adw::PreferencesGroup,
    wording: Wording,
    /// A sequence's row, or the row that adds one, was activated: the place of
    /// the sequence in the list, `None` for a new one.
    on_open: Box<dyn Fn(Option<usize>)>,
    /// The person picked an entry of the default row: see [`picked`](Self::picked).
    on_pick: Box<dyn Fn(Choice)>,
    rows: RefCell<Vec<adw::ActionRow>>,
    /// What the sequence rows show (each name and its number of commands), so
    /// that they are made again only when that changes, and a refresh that
    /// brings nothing new leaves the rows, and the keyboard on them, alone.
    shown: RefCell<Option<Vec<(String, usize)>>>,
    /// "Default sequence": "None", then each sequence by name. In the group
    /// between the sequence rows and "Add sequence" while there is a sequence.
    default_row: adw::ComboRow,
    /// The sequence names the row offers after "None", to make its list again
    /// only when they change.
    names: RefCell<Vec<String>>,
    /// The sequences as the owner last showed them from the file: what a pick
    /// means. A pick carries its sequence along, and the file refuses it if the
    /// list changed since (`form::CHOICE_CHANGED`).
    sequences: RefCell<Vec<StartupSequence>>,
    /// What the file says, or will say once the saves asked for are back: the
    /// last pick, while one is being saved.
    stored: Cell<Option<usize>>,
    saving: Rc<Saving>,
    /// The row is being set to what the file says (or its list is being made
    /// again, which moves the selection), not picked by the person: that saves
    /// nothing.
    loading: Cell<bool>,
}

impl SequenceRows {
    /// Rows for `group`, which the owner puts on its page; nothing is in the
    /// group until the first [`show`](Self::show). `on_open` and `on_pick` are
    /// held by the rows and must not hold the owner strongly.
    pub(super) fn new(
        group: &adw::PreferencesGroup,
        wording: Wording,
        on_open: impl Fn(Option<usize>) + 'static,
        on_pick: impl Fn(Choice) + 'static,
    ) -> Rc<SequenceRows> {
        // Its list is made by `show`; names are data: plain text.
        let default_row = adw::ComboRow::builder()
            .title("Default sequence")
            .subtitle(wording.default_subtitle)
            .use_markup(false)
            .factory(&plain_text_factory())
            .model(&gtk::StringList::new(&[DEFAULT_NONE]))
            .build();
        let saving = Saving::new();
        default_row.add_suffix(&saving.spinner);
        Rc::new_cyclic(|me: &Weak<SequenceRows>| {
            let picked = me.clone();
            default_row.connect_selected_notify(move |_| {
                if let Some(rows) = picked.upgrade() {
                    rows.picked();
                }
            });
            SequenceRows {
                me: me.clone(),
                group: group.clone(),
                wording,
                on_open: Box::new(on_open),
                on_pick: Box::new(on_pick),
                rows: RefCell::new(Vec::new()),
                shown: RefCell::new(None),
                default_row,
                names: RefCell::new(Vec::new()),
                sequences: RefCell::new(Vec::new()),
                stored: Cell::new(None),
                saving,
                loading: Cell::new(false),
            }
        })
    }

    /// The "Default sequence" row: what a pick is saved against (the owner
    /// marks it `error` and shows its message near it).
    pub(super) fn default_row(&self) -> &adw::ComboRow {
        &self.default_row
    }

    /// The default row's saves under way, and its spinner.
    pub(super) fn default_saving(&self) -> &Rc<Saving> {
        &self.saving
    }

    /// Show `sequences` and their `default` (the place of the one that runs
    /// every time, `None` for none): the sequence rows and the default row.
    /// The rows are made again only when what they show has changed.
    pub(super) fn show(&self, sequences: &[StartupSequence], default: Option<usize>) {
        let shown: Vec<(String, usize)> = sequences
            .iter()
            .map(|sequence| (sequence.name.clone(), sequence.commands.len()))
            .collect();
        if self.shown.borrow().as_ref() != Some(&shown) {
            self.make_rows(sequences);
            self.shown.replace(Some(shown));
        }
        self.show_default(sequences, default);
    }

    /// One row per sequence (name and number of commands), each opening the
    /// sequence's sub-page, then, if there is a sequence, the "Default
    /// sequence" row, then a row that adds one.
    fn make_rows(&self, sequences: &[StartupSequence]) {
        for row in self.rows.take() {
            self.group.remove(&row);
        }
        // The group knows only its own children: the default row is one of
        // them only if it was put there.
        if self.default_row.parent().is_some() {
            self.group.remove(&self.default_row);
        }
        let opens = |row: &adw::ActionRow, index: Option<usize>| {
            let weak = self.me.clone();
            row.connect_activated(move |_| {
                if let Some(rows) = weak.upgrade() {
                    (rows.on_open)(index);
                }
            });
        };
        let mut rows = Vec::new();
        for (index, sequence) in sequences.iter().enumerate() {
            let count = sequence.commands.len();
            let subtitle = if count == 1 {
                "1 command".to_owned()
            } else {
                format!("{count} commands")
            };
            let row = adw::ActionRow::builder()
                .title(sequence.name.as_str())
                .subtitle(subtitle)
                .use_markup(false)
                .activatable(true)
                .build();
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            opens(&row, Some(index));
            rows.push(row);
        }
        if rows.is_empty() {
            rows.push(
                adw::ActionRow::builder()
                    .title(self.wording.empty)
                    .use_markup(false)
                    .css_classes(["dim-label"])
                    .build(),
            );
        }
        let add = adw::ActionRow::builder()
            .title("Add sequence")
            .use_markup(false)
            .activatable(true)
            .build();
        add.add_prefix(&gtk::Image::from_icon_name("list-add-symbolic"));
        opens(&add, None);
        rows.push(add);
        // The sequences (or the note that there are none), the default row
        // when there is a sequence to choose, and the row that adds one.
        let last = rows.len() - 1;
        for (place, row) in rows.iter().enumerate() {
            if place == last && !sequences.is_empty() {
                self.group.add(&self.default_row);
            }
            self.group.add(row);
        }
        self.rows.replace(rows);
    }

    /// Show the default sequence in its row, and make the row's list again if
    /// the sequence names changed. Nothing here counts as a pick: the row
    /// reports only what the person chooses ([`picked`](Self::picked)). While a
    /// pick is being saved the row keeps showing it; once it is back the next
    /// call shows the file's choice, which puts a failed pick back.
    fn show_default(&self, sequences: &[StartupSequence], default: Option<usize>) {
        if !self.saving.busy() {
            self.stored.set(default);
        }
        let names: Vec<String> = sequences
            .iter()
            .map(|sequence| sequence.name.clone())
            .collect();
        self.sequences.replace(sequences.to_vec());
        self.loading.set(true);
        if *self.names.borrow() != names {
            let items: Vec<&str> = std::iter::once(DEFAULT_NONE)
                .chain(names.iter().map(String::as_str))
                .collect();
            self.default_row
                .set_model(Some(&gtk::StringList::new(&items)));
            self.names.replace(names);
        }
        // A pick that names a place the list no longer has shows as "None".
        let place = self
            .stored
            .get()
            .filter(|&index| index < self.names.borrow().len())
            .map_or(0, |index| index + 1);
        let place = u32::try_from(place).unwrap_or(0);
        if self.default_row.selected() != place {
            self.default_row.set_selected(place);
        }
        self.loading.set(false);
    }

    /// The person picked an entry of the "Default sequence" row: pass it on, as
    /// the sequence's place in the list together with the sequence the person
    /// saw there (`None` for "None"). The file has the last word: unless it
    /// has that very sequence at that place (a sync took it away, moved it or
    /// changed it since the page showed it) the owner's save is refused
    /// (`Field::StartupSequence`), nothing is written, and the row goes back
    /// to what the file says.
    fn picked(&self) {
        if self.loading.get() {
            return;
        }
        let index = match self.default_row.selected() {
            gtk::INVALID_LIST_POSITION => return,
            0 => None,
            entry => Some(entry as usize - 1),
        };
        if index == self.stored.get() {
            return;
        }
        let choice = match index {
            None => None,
            // The row's list is made from these, so there is always one.
            Some(index) => match self.sequences.borrow().get(index) {
                Some(sequence) => Some((index, sequence.clone())),
                None => return,
            },
        };
        self.stored.set(index);
        (self.on_pick)(choice);
    }
}
