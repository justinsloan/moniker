//! The choices a "›" opens in the new-tab list: for a profile ([`choices`]),
//! "Just connect", each startup sequence, and "Save transcript"; for the Local
//! terminal ([`local_choices`]), "Just open" and each of its own startup
//! sequences (issue #13), or "Open" while it has none, and "Save transcript".
//!
//! A click (or Enter) on a row opens it at once, with "Save transcript" checked
//! if the profile (or Local terminal) says so or "Save transcripts of all
//! sessions" is on ([`crate::terminal_tab::default_save`]), and running its
//! default sequence if it has one (a profile's: issue #12; Local terminal's:
//! issue #13). These choices are the way round that: "Just connect" and "Just
//! open" skip the default and run no sequence, and each sequence by name runs
//! that one, whichever is the default (the default is not marked here). "Just"
//! says "without a sequence", which only means something when there are
//! sequences: Local terminal without any offers "Open", as before; a profile's
//! choices say "Just connect" always. The choices are for every profile, with
//! or without sequences, and for the Local terminal, so the choice of saving
//! one session's transcript is reachable by keyboard.
//!
//! Profile names and sequence names come from other machines through sync, and
//! Local terminal's from a file that can be edited by hand: every
//! label here has Pango markup off.

use std::rc::Rc;

use gtk::prelude::*;
use gtk::{glib, pango};
use gtk4 as gtk;
use moniker_core::profile::{Profile, StartupSequence};

/// Label of the transcript check box in the connect choices.
pub(crate) const RECORD_LABEL: &str = "Save transcript";

/// Label of the button of the Local terminal's choices while it has no startup
/// sequence.
pub(crate) const OPEN_LABEL: &str = "Open";

/// Label of the Local terminal's first choice while it has startup sequences:
/// a shell, without running any of them, the default included.
pub(crate) const JUST_OPEN_LABEL: &str = "Just open";

/// Widget name of the column [`choices`] returns (tests look for it).
pub(crate) const CHOICES_NAME: &str = "connect-choices";

/// Widget name of the column [`local_choices`] returns (tests look for it).
pub(crate) const LOCAL_CHOICES_NAME: &str = "local-choices";

/// What the choices offer for `profile`: "Just connect" (`None`), then each
/// startup sequence by name (`Some(index)`).
fn entries(profile: &Profile) -> Vec<(String, Option<usize>)> {
    std::iter::once(("Just connect".to_owned(), None))
        .chain(
            profile
                .sequences
                .iter()
                .enumerate()
                .map(|(index, sequence)| (sequence.name.clone(), Some(index))),
        )
        .collect()
}

/// What the Local terminal's choices offer for its startup `sequences`: with
/// none, "Open" alone (`None`); with some, "Just open" (`None`), then each
/// sequence by name, carrying the sequence itself, so what runs is what was
/// shown even if its file changes before the click.
fn local_entries(sequences: &[StartupSequence]) -> Vec<(String, Option<StartupSequence>)> {
    if sequences.is_empty() {
        return vec![(OPEN_LABEL.to_owned(), None)];
    }
    std::iter::once((JUST_OPEN_LABEL.to_owned(), None))
        .chain(
            sequences
                .iter()
                .map(|sequence| (sequence.name.clone(), Some(sequence.clone()))),
        )
        .collect()
}

/// A flat button called `label`, plain text. (The Layouts menu's buttons are
/// made the same way.)
pub(crate) fn entry_button(label: &str) -> gtk::Button {
    let label = gtk::Label::builder()
        .label(label)
        .use_markup(false)
        .xalign(0.0)
        .ellipsize(pango::EllipsizeMode::End)
        .max_width_chars(40)
        .build();
    gtk::Button::builder()
        .child(&label)
        .has_frame(false)
        .build()
}

/// The "Save transcript" check box, checked if `save`.
fn save_box(save: bool) -> gtk::CheckButton {
    gtk::CheckButton::builder()
        .label(RECORD_LABEL)
        .active(save)
        .margin_start(6)
        .margin_end(6)
        .margin_top(2)
        .margin_bottom(4)
        .build()
}

/// The column of `buttons`, a separator and the check box `record`. The first
/// button takes the focus when the column is shown, so Enter picks it.
fn column(name: &str, buttons: &[gtk::Button], record: &gtk::CheckButton) -> gtk::Box {
    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .name(name)
        .build();
    for button in buttons {
        column.append(button);
    }
    column.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    column.append(record);
    column.connect_map(|column| {
        if let Some(first) = column.first_child() {
            first.grab_focus();
        }
    });
    column
}

/// A column with one button per [`entries`] entry, then a "Save transcript"
/// check box that starts out as `save`: the caller works that out from the
/// profile's own setting and "Save transcripts of all sessions"
/// ([`crate::terminal_tab::default_save`]).
///
/// Clicking a button calls `on_choose` with that entry's choice and with
/// whether the box is checked: that is the connect-time override of the
/// profile's transcript setting. The labels are plain text: sequence names
/// come from profile data. "Just connect" takes the focus when the column is
/// shown, so Enter connects.
pub fn choices(
    profile: &Profile,
    save: bool,
    on_choose: impl Fn(Option<usize>, bool) + 'static,
) -> gtk::Widget {
    let on_choose = Rc::new(on_choose);
    let record = save_box(save);
    let buttons: Vec<gtk::Button> = entries(profile)
        .into_iter()
        .map(|(name, choice)| {
            let button = entry_button(&name);
            button.connect_clicked(glib::clone!(
                #[weak]
                record,
                #[strong]
                on_choose,
                move |_| on_choose(choice, record.is_active())
            ));
            button
        })
        .collect();
    column(CHOICES_NAME, &buttons, &record).upcast()
}

/// The Local terminal's choices for its startup `sequences`: a button per
/// [`local_entries`] entry ("Open" alone while there is none; else "Just open",
/// then each sequence by name), then the "Save transcript" check box, checked
/// if `save` (the caller works that out from Local terminal's own switch and
/// "Save transcripts of all sessions", [`crate::terminal_tab::default_save`]).
///
/// Clicking a button calls `on_open` with the sequence to run (`None` for "Open"
/// and "Just open": a plain shell) and whether the box is checked. The first
/// button takes the focus when the column is shown, so Enter opens a plain
/// shell. The labels are plain text: the names are the person's own, from a
/// file that can be edited by hand.
pub fn local_choices(
    sequences: &[StartupSequence],
    save: bool,
    on_open: impl Fn(Option<StartupSequence>, bool) + 'static,
) -> gtk::Widget {
    let on_open = Rc::new(on_open);
    let record = save_box(save);
    let buttons: Vec<gtk::Button> = local_entries(sequences)
        .into_iter()
        .map(|(name, sequence)| {
            let button = entry_button(&name);
            button.connect_clicked(glib::clone!(
                #[weak]
                record,
                #[strong]
                on_open,
                move |_| on_open(sequence.clone(), record.is_active())
            ));
            button
        })
        .collect();
    column(LOCAL_CHOICES_NAME, &buttons, &record).upcast()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sequence(name: &str) -> StartupSequence {
        StartupSequence {
            name: name.to_owned(),
            commands: vec!["uptime".to_owned()],
        }
    }

    #[test]
    fn the_choices_are_just_connect_then_each_sequence_in_order() {
        let mut p = Profile::new("web", "example.com", "alice");
        p.sequences = vec![sequence("update"), sequence("<b>x</b> & _y_")];
        assert_eq!(
            entries(&p),
            [
                ("Just connect".to_owned(), None),
                ("update".to_owned(), Some(0)),
                ("<b>x</b> & _y_".to_owned(), Some(1)),
            ]
        );
    }

    #[test]
    fn a_profile_without_sequences_has_only_just_connect() {
        let p = Profile::new("web", "example.com", "alice");
        assert_eq!(entries(&p), [("Just connect".to_owned(), None)]);
    }

    #[test]
    fn local_terminal_without_sequences_offers_open() {
        assert_eq!(local_entries(&[]), [("Open".to_owned(), None)]);
        assert_eq!(OPEN_LABEL, "Open");
    }

    #[test]
    fn local_terminal_offers_just_open_then_each_sequence_in_order() {
        let update = sequence("update");
        let odd = sequence("<b>x</b> & _y_");
        assert_eq!(
            local_entries(&[update.clone(), odd.clone()]),
            [
                ("Just open".to_owned(), None),
                ("update".to_owned(), Some(update)),
                ("<b>x</b> & _y_".to_owned(), Some(odd)),
            ]
        );
        assert_eq!(JUST_OPEN_LABEL, "Just open");
    }
}
