//! The startup-sequence sub-page: one sequence of a profile, pushed over the
//! profile page in the content pane ([`push`]).
//!
//! A Name row and a monospace editor with one command per line. A multi-line
//! editor has no ✓, so the header has Cancel and Save. Save, Move Up, Move
//! Down and Delete Sequence all go through [`form::apply_sequences`]: the list
//! they save is built, under the profile file's lock, from the sequences the
//! file holds *then*, not from what the page was opened on, and only if the
//! sequence at its place is still the one the page was opened on. A sync that
//! reordered or changed the list meanwhile is therefore never overwritten, and
//! Move or Delete never acts on a different sequence.
//!
//! The profile's default sequence is kept by position, so each of them says
//! how the default follows, and the same write saves both: Save leaves it, Move
//! takes it along with its sequence ([`form::moved_default`]), and Delete
//! clears it or moves it up by one ([`form::default_without`]).
//!
//! Move and Delete act on what is saved, at once. Text typed in the editor is
//! kept by Move (the page stays open at the sequence's new place) and is
//! discarded by Cancel, Escape, Delete, or selecting another profile.
//!
//! Each of them runs on the keyring worker ([`keyring::write_profiles`]), never
//! on the main thread (issue #7): a sync that waits for the keyring holds the
//! profile file. Until it is back the page takes no typing and no other Save,
//! Move or Delete, Cancel and Escape wait too, and after a moment a spinner in
//! the header says it waits. Selecting another profile still closes the page;
//! the change asked for goes on, and a failure is then told in the window.
//!
//! The page does not follow the profile file: a refresh of the profile page
//! under it leaves what was typed alone. If the profile is deleted by a sync
//! while the page is open, Save fails with the usual message and writes
//! nothing.
//!
//! Commands can hold secrets, so no message here quotes one: errors come from
//! [`form`] and name the rule, not the text.

use std::cell::Cell;
use std::rc::{Rc, Weak};

use adw::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;
use moniker_core::profile::{Profile, StartupSequence};

use super::page::SECRETS_WARNING;
use super::saving::Saving;
use super::{PROFILE_TAG, SEQUENCE_TAG};
use crate::context::AppContext;
use crate::form::{self, Move};
use crate::keyring;

/// The page's title and the header's. They name no sequence: the name is data,
/// shown in the Name row with markup off.
const TITLE: &str = "Startup Sequence";

/// The same, for a sequence that is not saved yet.
const NEW_TITLE: &str = "New Startup Sequence";

/// Widgets and state of an open sub-page.
struct Inner {
    ctx: Rc<AppContext>,
    /// The profile whose sequence this is.
    id: String,
    nav: glib::WeakRef<adw::NavigationView>,
    /// Where the sequence sits in the profile's list; `None` for a new one.
    /// Move changes it.
    index: Cell<Option<usize>>,
    /// The sequence as it was when the page was opened (`None` for a new
    /// one): what [`form::apply_sequences`] expects to find at `index`.
    original: Option<StartupSequence>,
    /// How many sequences the profile had at the last read: Move Down is
    /// offered while there is a later one.
    count: Cell<usize>,
    name: adw::EntryRow,
    commands: gtk::TextView,
    error: gtk::Label,
    up: gtk::Button,
    down: gtk::Button,
    cancel: gtk::Button,
    save: gtk::Button,
    delete: gtk::Button,
    /// The change under way, if any (module docs).
    saving: Rc<Saving>,
    /// The page itself, once it is made: it cannot be left while a change is
    /// under way.
    page: glib::WeakRef<adw::NavigationPage>,
}

/// Push the sub-page for the sequence at `index` of profile `id` (`None`: a new
/// sequence) onto `nav`.
///
/// The sequence is read from the file as it is now. If it cannot be (the
/// profile or the sequence is gone) the message is returned and nothing is
/// pushed.
pub(super) fn push(
    nav: &adw::NavigationView,
    ctx: &Rc<AppContext>,
    id: &str,
    index: Option<usize>,
) -> Result<(), String> {
    // Bind first so the store borrow ends here.
    let reloaded = ctx.store.borrow_mut().reload();
    reloaded.map_err(|err| format!("Could not read the profile file: {err}"))?;
    let current = form::sequences_of(&ctx.store.borrow(), id)?;
    let original = match index {
        None => None,
        Some(i) => Some(
            current
                .get(i)
                .cloned()
                .ok_or_else(|| form::SEQUENCE_CHANGED.to_owned())?,
        ),
    };

    let name = adw::EntryRow::builder()
        .title("Name")
        .use_markup(false)
        .text(original.as_ref().map_or("", |s| s.name.as_str()))
        .build();
    let commands = gtk::TextView::builder()
        .monospace(true)
        .wrap_mode(gtk::WrapMode::WordChar)
        // Tab moves on, as in every other field; it is not a command character.
        .accepts_tab(false)
        .input_hints(gtk::InputHints::NO_SPELLCHECK | gtk::InputHints::NO_EMOJI)
        .top_margin(8)
        .bottom_margin(8)
        .left_margin(8)
        .right_margin(8)
        .build();
    commands.update_property(&[gtk::accessible::Property::Label("Commands")]);
    if let Some(original) = &original {
        commands
            .buffer()
            .set_text(&form::format_commands(&original.commands));
    }
    let error = gtk::Label::builder()
        .wrap(true)
        .xalign(0.0)
        .css_classes(["error"])
        .accessible_role(gtk::AccessibleRole::Alert)
        .visible(false)
        .build();

    let inner = Rc::new(Inner {
        ctx: Rc::clone(ctx),
        id: id.to_owned(),
        nav: nav.downgrade(),
        index: Cell::new(index),
        original,
        count: Cell::new(current.len()),
        name,
        commands,
        error,
        up: gtk::Button::with_label("Move Up"),
        down: gtk::Button::with_label("Move Down"),
        cancel: gtk::Button::with_label("Cancel"),
        save: gtk::Button::builder()
            .label("Save")
            .css_classes(["suggested-action"])
            .build(),
        delete: gtk::Button::builder()
            .label("Delete Sequence")
            .halign(gtk::Align::Center)
            .css_classes(["destructive-action", "pill"])
            .build(),
        saving: Saving::new(),
        page: glib::WeakRef::new(),
    });
    inner.update_moves();

    let page = adw::NavigationPage::builder()
        .title(if index.is_some() { TITLE } else { NEW_TITLE })
        .tag(SEQUENCE_TAG)
        .child(&inner.build_view())
        .build();
    inner.page.set(Some(&page));
    inner.connect();

    // The page owns its state: this handler holds the only strong reference,
    // so the state goes when the page does. Once the page is out of sight
    // what was typed is dropped from the widgets.
    let keep = Rc::clone(&inner);
    page.connect_hidden(move |_| keep.discard());
    // Where to start typing.
    let first = if index.is_some() {
        inner.commands.clone().upcast::<gtk::Widget>()
    } else {
        inner.name.clone().upcast::<gtk::Widget>()
    };
    page.connect_shown(move |_| {
        first.grab_focus();
    });

    nav.push(&page);
    Ok(())
}

impl Inner {
    /// The page's widgets: a header with Cancel and Save, then the Name row,
    /// the editor with the secrets warning, and (for an existing sequence)
    /// the order and delete buttons.
    fn build_view(&self) -> gtk::Widget {
        let existing = self.index.get().is_some();
        let title = gtk::Label::builder()
            .label(if existing { TITLE } else { NEW_TITLE })
            .css_classes(["title"])
            .build();
        // No back button: Cancel is the way out, and it says what happens to
        // the text.
        let header = adw::HeaderBar::builder()
            .show_back_button(false)
            .title_widget(&title)
            .build();
        header.pack_start(&self.cancel);
        header.pack_end(&self.save);
        header.pack_end(&self.saving.spinner);

        let page = adw::PreferencesPage::new();
        let identity = adw::PreferencesGroup::new();
        identity.add(&self.name);
        page.add(&identity);

        let scroll = gtk::ScrolledWindow::builder()
            .child(&self.commands)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .min_content_height(180)
            .build();
        let editor = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .build();
        editor.append(&gtk::Frame::builder().child(&scroll).build());
        editor.append(&self.error);
        let commands = adw::PreferencesGroup::builder()
            .title("Commands, one per line")
            .description(SECRETS_WARNING)
            .build();
        commands.add(&editor);
        page.add(&commands);

        // Order and delete make no sense for a sequence that is not saved yet.
        let moves = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .halign(gtk::Align::Center)
            .css_classes(["linked"])
            .build();
        moves.append(&self.up);
        moves.append(&self.down);
        let order = adw::PreferencesGroup::builder()
            .title("Order")
            .description("The connect options list sequences in this order.")
            .visible(existing)
            .build();
        order.add(&moves);
        page.add(&order);
        let danger = adw::PreferencesGroup::builder().visible(existing).build();
        danger.add(&self.delete);
        page.add(&danger);

        let view = adw::ToolbarView::new();
        view.add_top_bar(&header);
        view.set_content(Some(&page));
        view.upcast()
    }

    /// Wire the buttons and fields. Every handler holds the state weakly: the
    /// page owns the widgets they are attached to.
    fn connect(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        let with = move |f: fn(&Rc<Inner>)| {
            let weak: Weak<Inner> = weak.clone();
            move || {
                if let Some(inner) = weak.upgrade() {
                    f(&inner);
                }
            }
        };

        let go = with(|inner| inner.close());
        self.cancel.connect_clicked(move |_| go());
        let go = with(|inner| inner.save());
        self.save.connect_clicked(move |_| go());
        let go = with(|inner| inner.move_by(Move::Up));
        self.up.connect_clicked(move |_| go());
        let go = with(|inner| inner.move_by(Move::Down));
        self.down.connect_clicked(move |_| go());
        let go = with(|inner| inner.delete());
        self.delete.connect_clicked(move |_| go());

        // Enter in the name goes on to the commands (Enter there is a new line).
        let go = with(|inner| {
            inner.commands.grab_focus();
        });
        self.name.connect_entry_activated(move |_| go());
        // An old message is about old text.
        let go = with(|inner| inner.error.set_visible(false));
        self.name.connect_changed(move |_| go());
        let go = with(|inner| inner.error.set_visible(false));
        self.commands.buffer().connect_changed(move |_| go());
    }

    /// The sequence as typed, or a message for the person.
    fn edited(&self) -> Result<StartupSequence, String> {
        let buffer = self.commands.buffer();
        let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
        Ok(StartupSequence {
            name: self.name.text().trim().to_owned(),
            commands: form::parse_commands(&text)?,
        })
    }

    /// Change the profile's sequences on the keyring worker (module docs):
    /// `change` gets the list as it is in the file then and returns the one to
    /// save, and `follow` gets the default sequence as the file has it then and
    /// returns the one to save with it. When it is back, `then` gets the
    /// profile as stored, or `None` if the list came out the same and nothing
    /// was written; a failure is shown ([`fail`](Self::fail)). If the page has
    /// gone by then, a failure is told in the window.
    fn apply(
        self: &Rc<Self>,
        change: impl FnOnce(&[StartupSequence]) -> Vec<StartupSequence> + Send + 'static,
        follow: impl FnOnce(Option<usize>) -> Option<usize> + Send + 'static,
        then: impl FnOnce(&Rc<Inner>, Option<Profile>) + 'static,
    ) {
        self.saving.begin();
        self.set_waiting(true);
        let id = self.id.clone();
        let target = self.index.get().zip(self.original.clone());
        let ctx = Rc::clone(&self.ctx);
        let saving = Rc::clone(&self.saving);
        let weak = Rc::downgrade(self);
        keyring::write_profiles(
            &self.ctx,
            move |store| {
                let target = target.as_ref().map(|(index, sequence)| (*index, sequence));
                form::apply_sequences(store, &id, target, change, follow)
            },
            move |result| {
                saving.end();
                let Some(inner) = weak.upgrade() else {
                    if let Err(message) = &result {
                        ctx.notice.show(message);
                    }
                    ctx.profiles_changed();
                    return;
                };
                inner.set_waiting(false);
                match result {
                    Ok(saved) => then(&inner, saved),
                    Err(message) => inner.fail(&message),
                }
            },
        );
    }

    /// While a change is under way (module docs): no typing, and no leaving
    /// by Cancel, Escape or the back gesture.
    fn set_waiting(&self, waiting: bool) {
        self.name.set_editable(!waiting);
        self.commands.set_editable(!waiting);
        self.cancel.set_sensitive(!waiting);
        if let Some(page) = self.page.upgrade() {
            page.set_can_pop(!waiting);
        }
    }

    /// Show a failure on the page (the text stays, the page stays) and as a
    /// notification. When the file turned out to differ from what the window
    /// shows, the pages behind this one are brought up to date too, so Cancel
    /// lands on the current list.
    fn fail(&self, message: &str) {
        self.error.set_label(message);
        self.error.set_visible(true);
        self.ctx.notice.show_near(&self.error, message);
        if message == form::GONE || message == form::SEQUENCE_CHANGED {
            self.ctx.profiles_changed();
        }
    }

    /// Back to the profile page. The text is dropped by [`discard`](Self::discard)
    /// once the page is out of sight.
    fn close(&self) {
        if let Some(nav) = self.nav.upgrade() {
            nav.pop_to_tag(PROFILE_TAG);
        }
    }

    /// Drop what was typed from the widgets (commands can hold secrets).
    fn discard(&self) {
        self.commands.buffer().set_text("");
        self.name.set_text("");
    }

    fn save(self: &Rc<Self>) {
        if self.saving.busy() {
            return;
        }
        let sequence = match self.edited() {
            Ok(sequence) => sequence,
            Err(message) => return self.fail(&message),
        };
        let index = self.index.get();
        self.apply(
            move |current| form::with_sequence(current, index, sequence),
            |default| default,
            |inner, _| {
                inner.ctx.profiles_changed();
                inner.close();
            },
        );
    }

    fn move_by(self: &Rc<Self>, dir: Move) {
        let Some(index) = self.index.get() else {
            return;
        };
        if self.saving.busy() {
            return;
        }
        self.apply(
            move |current| form::moved(current, index, dir),
            move |default| form::moved_default(default, index, dir),
            move |inner, saved| {
                if let Some(profile) = saved {
                    // It moved: the sequence now sits one place over.
                    inner.index.set(Some(match dir {
                        Move::Up => index.saturating_sub(1),
                        Move::Down => index + 1,
                    }));
                    inner.count.set(profile.sequences.len());
                    inner.ctx.profiles_changed();
                }
                inner.update_moves();
            },
        );
    }

    fn delete(self: &Rc<Self>) {
        let Some(index) = self.index.get() else {
            return;
        };
        if self.saving.busy() {
            return;
        }
        self.apply(
            move |current| form::without_sequence(current, index),
            move |default| form::default_without(default, index),
            |inner, _| {
                inner.ctx.profiles_changed();
                inner.close();
            },
        );
    }

    /// Move Up needs an earlier sequence, Move Down a later one.
    fn update_moves(&self) {
        let index = self.index.get();
        self.up.set_sensitive(index.is_some_and(|i| i > 0));
        self.down
            .set_sensitive(index.is_some_and(|i| i + 1 < self.count.get()));
    }
}
