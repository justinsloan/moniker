//! Local terminal's own startup sequences, default sequence and "Save
//! transcript" switch (issue #13): the data, its place in the settings file
//! and its edits. Nothing here touches GTK.
//!
//! A profile keeps its sequences in `profiles.json`, which syncs. Local
//! terminal's must never leave this machine, or any machine holding the sync
//! key could put commands into this one's shells, so they are not a profile
//! and not in that file: they are one field of the GUI settings file
//! ([`settings`], `gui-settings.json`: this machine's own, private, never
//! synced), `"local_terminal": {"sequences": [...], "startup_sequence": N,
//! "transcript": bool}`, left out of the file while it holds nothing. The
//! settings file's writer keeps every other setting as it was, and refuses to
//! write over a file it cannot read ([`settings::update`]); the changes here
//! go through it, so they refuse in the same way.
//!
//! It is read field by field, as the settings file is. A missing, null or odd
//! field is that field's default. A sequence that breaks a profile's rules
//! ([`StartupSequence::check`]) or is not of the right shape is skipped, which
//! only a hand edit can cause; the default is a place in the list, so it
//! stays on the sequence the file named while the entries before it are
//! skipped, and is none if its own was. The default is an index, and one past
//! the end of the list names nothing ([`LocalTerminal::startup_choice`]), as a
//! profile's does.
//!
//! The edits mirror a profile's ([`crate::form::apply_sequences`] and the
//! default's pick): the change is built from the list the file holds when it
//! is saved, only if the sequence at its place is still the one the page was
//! opened on, and a pick of the default carries the sequence it means. Both
//! are refused otherwise, with the profile's texts, and nothing is written.

use std::path::Path;

use moniker_core::profile::StartupSequence;
use serde::Serialize;
use serde_json::Value;

use crate::form;
use crate::settings;

/// What the page for Local terminal says about what its sequences and
/// transcripts may hold. A profile's page says the same about its own.
pub const LOCAL_SECRETS_WARNING: &str = "Startup commands are saved as plain text in Moniker's settings file, and a transcript records whatever appears in the terminal, including anything you type or paste. Keep passwords out of both.";

/// The description of the Startup sequences group on Local terminal's page:
/// where they run, that they are never synced, and [`LOCAL_SECRETS_WARNING`].
pub const LOCAL_SEQUENCES_NOTE: &str = "These run in a shell on this computer when you open Local terminal, and are never synced. Startup commands are saved as plain text in Moniker's settings file, and a transcript records whatever appears in the terminal, including anything you type or paste. Keep passwords out of both.";

/// Local terminal's sequences, default and transcript switch, as the settings
/// file holds them. The default value (no sequences, no default, transcript
/// off) is what a machine that never used any of this has, and is not written.
///
/// `Debug` prints every command, and commands can hold secrets: do not log it.
#[derive(Serialize, Clone, Debug, PartialEq, Default)]
pub struct LocalTerminal {
    /// Every sequence is one [`StartupSequence::check`] accepts.
    pub sequences: Vec<StartupSequence>,
    /// The sequence a plain pick of Local terminal runs, as an index into
    /// `sequences`. May point past the end: see [`LocalTerminal::startup_choice`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup_sequence: Option<usize>,
    /// Save a transcript of every local tab (the switch on Local terminal's
    /// page; "Save transcripts of all sessions" also does).
    pub transcript: bool,
}

impl LocalTerminal {
    /// The local terminal in the settings field `value`, which is anything the
    /// file might hold under `"local_terminal"` (or nothing). Each of its three
    /// fields is read on its own and falls back to its default: a missing one,
    /// a null, or one of the wrong type costs the others nothing.
    pub fn from_value(value: Option<&Value>) -> LocalTerminal {
        let Some(fields) = value.and_then(Value::as_object) else {
            return LocalTerminal::default();
        };
        let entries: &[Value] = fields
            .get("sequences")
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice);
        let usable: Vec<Option<StartupSequence>> = entries
            .iter()
            .map(|entry| {
                serde_json::from_value::<StartupSequence>(entry.clone())
                    .ok()
                    .filter(|seq| seq.check().is_ok())
            })
            .collect();
        // The default's place in the file's list, then its place once the
        // entries that cannot be used are taken out: still on its own
        // sequence, or none if that one was taken out. One past the end of
        // the file's list stays past the end of what is left.
        let startup_sequence = fields
            .get("startup_sequence")
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .and_then(|n| match usable.get(n) {
                None => Some(n),
                Some(entry) => entry
                    .is_some()
                    .then(|| usable[..n].iter().flatten().count()),
            });
        LocalTerminal {
            sequences: usable.into_iter().flatten().collect(),
            startup_sequence,
            transcript: fields
                .get("transcript")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }
    }

    /// The index of the sequence a plain pick runs: the saved default when it
    /// points at one of `sequences`, else `None`. An index past the end (a hand
    /// edit) runs nothing, and does not come to life when the list grows.
    pub fn startup_choice(&self) -> Option<usize> {
        self.startup_sequence
            .filter(|&index| index < self.sequences.len())
    }

    /// The sequence a plain pick runs ([`LocalTerminal::startup_choice`]).
    pub fn default_sequence(&self) -> Option<&StartupSequence> {
        self.startup_choice()
            .and_then(|index| self.sequences.get(index))
    }

    /// Whether this is what a machine that never used Local terminal's
    /// sequences has, which the settings file does not write.
    pub fn is_default(&self) -> bool {
        *self == LocalTerminal::default()
    }
}

/// What a failed save says, whatever failed: the settings file could not be
/// read or written ([`settings::update`]).
fn save_error(err: &std::io::Error) -> String {
    format!("Could not save the local terminal: {err}")
}

/// Change Local terminal's sequences in the settings file at `settings_path`
/// as the file holds them now: `change` gets the list and returns the one to
/// save.
///
/// The sub-page works on one sequence, opened at `target` (its index and what
/// it held then). The file can change while the page is open (another window,
/// a hand edit), and the index could then name a different sequence: moving or
/// deleting that one would be a silent loss, and saving a list built from an
/// older one would undo the edit. So the check and the change are made on what
/// the file holds: the sequence at that index must still be the one the page
/// was opened on, else [`form::SEQUENCE_CHANGED`] and nothing is written. A new
/// sequence (`None`) has no such expectation.
///
/// The default is kept by position, so a change to the list must carry it
/// along, in the same write: `follow` gets the default as the file holds it
/// ([`LocalTerminal::startup_choice`]: one that points past the end is `None`,
/// so it is not brought to life by a longer list) and returns the one to save
/// ([`form::moved_default`], [`form::default_without`], or `|d| d` for an edit
/// or an added sequence). It is not asked when the list comes out the same, and
/// what it returns is dropped if it names a place the new list does not have.
///
/// A list holding a sequence that [`StartupSequence::check`] refuses is not
/// saved (the reader would skip it at the next launch): the refusal says why.
///
/// `Ok(Some(n))`: written, with `n` sequences. `Ok(None)`: the list came out
/// the same, and nothing was written. An `Err` is a message for the person,
/// either of the refusals above or "Could not save the local terminal: …" when
/// the settings file cannot be read or written (a file that exists but cannot
/// be read is left as it is).
pub fn apply_sequences(
    settings_path: &Path,
    target: Option<(usize, &StartupSequence)>,
    change: impl FnOnce(&[StartupSequence]) -> Vec<StartupSequence>,
    follow: impl FnOnce(Option<usize>) -> Option<usize>,
) -> Result<Option<usize>, String> {
    let mut refusal = None;
    let mut written = None;
    settings::update(settings_path, |settings| {
        let local = &mut settings.local_terminal;
        if let Some((index, expected)) = target
            && local.sequences.get(index) != Some(expected)
        {
            refusal = Some(form::SEQUENCE_CHANGED.to_owned());
            return;
        }
        let next = change(&local.sequences);
        if next == local.sequences {
            return;
        }
        if let Some(err) = next.iter().find_map(|seq| seq.check().err()) {
            refusal = Some(form::describe(err));
            return;
        }
        local.startup_sequence = follow(local.startup_choice()).filter(|&d| d < next.len());
        local.sequences = next;
        written = Some(local.sequences.len());
    })
    .map_err(|err| save_error(&err))?;
    match refusal {
        Some(refusal) => Err(refusal),
        None => Ok(written),
    }
}

/// Make `choice` the sequence a plain pick of Local terminal runs, or none
/// (`None`: always allowed, and it also clears a default that named nothing).
///
/// A pick carries the sequence it means as well as its place in the list (the
/// page's view of both), and is refused with [`form::CHOICE_CHANGED`], writing
/// nothing, unless the file has that very sequence (name and commands) at that
/// place now: the default is kept by position, so a list changed since the
/// page showed it would otherwise make the position name another sequence,
/// and a pick must never run a sequence the person did not choose. The message
/// quotes no command.
pub fn set_default(
    settings_path: &Path,
    choice: Option<(usize, StartupSequence)>,
) -> Result<(), String> {
    let mut refusal = None;
    settings::update(settings_path, |settings| {
        let local = &mut settings.local_terminal;
        match &choice {
            None => local.startup_sequence = None,
            Some((index, expected)) if local.sequences.get(*index) == Some(expected) => {
                local.startup_sequence = Some(*index);
            }
            Some(_) => refusal = Some(form::CHOICE_CHANGED.to_owned()),
        }
    })
    .map_err(|err| save_error(&err))?;
    match refusal {
        Some(refusal) => Err(refusal),
        None => Ok(()),
    }
}

/// Turn Local terminal's own "Save transcript" switch on or off, keeping every
/// other setting as the file has it.
pub fn set_transcript(settings_path: &Path, on: bool) -> Result<(), String> {
    settings::update(settings_path, |settings| {
        settings.local_terminal.transcript = on;
    })
    .map_err(|err| save_error(&err))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::form::{self, Move};
    use crate::settings::{self, GuiSettings};
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;

    fn seq(name: &str, commands: &[&str]) -> StartupSequence {
        StartupSequence {
            name: name.to_owned(),
            commands: commands.iter().map(|c| (*c).to_owned()).collect(),
        }
    }

    fn hello() -> StartupSequence {
        seq("Hello", &["printf hi"])
    }

    fn second() -> StartupSequence {
        seq("Second", &["uptime"])
    }

    /// A settings path in a fresh directory (nothing written yet).
    fn world() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(settings::FILE_NAME);
        (dir, path)
    }

    /// Save `local` as the local terminal of the settings file at `path`.
    fn seed(path: &Path, local: LocalTerminal) {
        settings::save(
            path,
            &GuiSettings {
                local_terminal: local,
                ..GuiSettings::default()
            },
        )
        .unwrap();
    }

    fn two_with_default(default: Option<usize>) -> LocalTerminal {
        LocalTerminal {
            sequences: vec![hello(), second()],
            startup_sequence: default,
            transcript: false,
        }
    }

    #[test]
    fn missing_or_odd_is_the_default() {
        assert_eq!(LocalTerminal::from_value(None), LocalTerminal::default());
        for odd in [json!(null), json!(7), json!("x"), json!([])] {
            assert_eq!(
                LocalTerminal::from_value(Some(&odd)),
                LocalTerminal::default(),
                "{odd}"
            );
        }
        assert!(LocalTerminal::default().is_default());
    }

    #[test]
    fn each_field_falls_back_on_its_own() {
        let value = json!({"sequences": "x", "startup_sequence": 0, "transcript": true});
        let local = LocalTerminal::from_value(Some(&value));
        assert!(local.sequences.is_empty());
        assert_eq!(local.startup_sequence, Some(0));
        assert_eq!(local.startup_choice(), None, "nothing for it to name");
        assert!(local.transcript);

        for bad in [json!(-1), json!(1.5), json!("0"), json!(null), json!(true)] {
            let value =
                json!({"sequences": [hello()], "startup_sequence": bad, "transcript": true});
            let local = LocalTerminal::from_value(Some(&value));
            assert_eq!(local.startup_sequence, None, "{bad}");
            assert_eq!(local.sequences, [hello()], "{bad}: the others stay");
            assert!(local.transcript, "{bad}: the others stay");
        }
        for bad in [json!("yes"), json!(1), json!(null), json!([])] {
            let value = json!({"sequences": [hello()], "startup_sequence": 0, "transcript": bad});
            let local = LocalTerminal::from_value(Some(&value));
            assert!(!local.transcript, "{bad}");
            assert_eq!(local.sequences, [hello()], "{bad}: the others stay");
            assert_eq!(local.startup_sequence, Some(0), "{bad}: the others stay");
        }
    }

    #[test]
    fn entries_it_cannot_use_are_skipped() {
        let value = json!({"sequences": [
            hello(),
            {"name": "", "commands": ["a"]},
            {"name": "a\u{1b}b", "commands": ["a"]},
            {"name": "x", "commands": ["a\nb"]},
            {"name": "x", "commands": ["a\u{0}b"]},
            {"name": "y", "commands": [1]},
            {"name": "z"},
            5,
        ]});
        let local = LocalTerminal::from_value(Some(&value));
        assert_eq!(local.sequences, [hello()]);
    }

    #[test]
    fn a_skipped_entry_does_not_move_the_default_to_another_sequence() {
        // The default is a place in the list, and a list with an entry taken
        // out has its places changed: the default stays on the sequence the
        // file named, or on none if that one was taken out.
        let bad = json!({"name": "", "commands": ["a"]});
        let list = |default: u64| json!({"sequences": [hello(), bad, second()], "startup_sequence": default});
        let read = |default: u64| LocalTerminal::from_value(Some(&list(default)));
        assert_eq!(read(0).default_sequence(), Some(&hello()));
        assert_eq!(read(1).default_sequence(), None, "its own entry is gone");
        assert_eq!(read(1).startup_choice(), None);
        assert_eq!(read(2).default_sequence(), Some(&second()));
        assert_eq!(read(2).startup_sequence, Some(1));
        assert_eq!(
            read(3).default_sequence(),
            None,
            "past the end of the file's"
        );
        assert_eq!(read(3).startup_choice(), None);
    }

    #[test]
    fn the_default_is_bounded() {
        let value = json!({"sequences": [hello(), second()], "startup_sequence": 3});
        let local = LocalTerminal::from_value(Some(&value));
        assert_eq!(local.startup_choice(), None);
        assert_eq!(local.default_sequence(), None);
        let value = json!({"sequences": [hello(), second()], "startup_sequence": 1});
        let local = LocalTerminal::from_value(Some(&value));
        assert_eq!(local.startup_choice(), Some(1));
        assert_eq!(local.default_sequence(), Some(&second()));
    }

    #[test]
    fn the_warning_and_the_note_are_the_agreed_words() {
        assert_eq!(
            LOCAL_SECRETS_WARNING,
            "Startup commands are saved as plain text in Moniker's settings file, \
             and a transcript records whatever appears in the terminal, including \
             anything you type or paste. Keep passwords out of both."
        );
        assert_eq!(
            LOCAL_SEQUENCES_NOTE,
            format!(
                "These run in a shell on this computer when you open Local terminal, \
                 and are never synced. {LOCAL_SECRETS_WARNING}"
            )
        );
    }

    #[test]
    fn it_round_trips_and_is_left_out_while_default() {
        let (_dir, path) = world();
        let local = LocalTerminal {
            sequences: vec![hello()],
            startup_sequence: Some(0),
            transcript: true,
        };
        seed(&path, local.clone());
        assert_eq!(settings::load(&path).local_terminal, local);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains(r#""local_terminal""#), "{text}");

        // The default is not written at all, so a file that never used the
        // local terminal reads as it did before.
        settings::save(&path, &GuiSettings::default()).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("local_terminal"), "{text}");
        assert_eq!(settings::load(&path), GuiSettings::default());

        // No default sequence is no key, not a null.
        seed(
            &path,
            LocalTerminal {
                sequences: vec![hello()],
                ..LocalTerminal::default()
            },
        );
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("startup_sequence"), "{text}");
    }

    #[test]
    fn other_settings_writes_keep_it() {
        let (_dir, path) = world();
        let local = LocalTerminal {
            sequences: vec![hello(), second()],
            startup_sequence: Some(1),
            transcript: true,
        };
        seed(&path, local.clone());

        settings::save_font_scale(&path, 1.5).unwrap();
        assert_eq!(settings::load(&path).local_terminal, local, "text size");
        assert_eq!(settings::load(&path).font_scale, 1.5);

        settings::update(&path, |s| s.ping_hosts = false).unwrap();
        assert_eq!(settings::load(&path).local_terminal, local, "a switch");
        assert!(!settings::load(&path).ping_hosts);
    }

    #[test]
    fn apply_sequences_follows_the_file() {
        let (_dir, path) = world();
        let bytes = || fs::read(&path).unwrap();

        // A missing file starts from the defaults: adding is the first write.
        let added = apply_sequences(
            &path,
            None,
            |current| form::with_sequence(current, None, hello()),
            |default| default,
        );
        assert_eq!(added, Ok(Some(1)));
        assert_eq!(settings::load(&path).local_terminal.sequences, [hello()]);

        // The page was opened on another sequence than the file holds.
        let before = bytes();
        let other = seq("Other", &["ls"]);
        let refused = apply_sequences(
            &path,
            Some((0, &other)),
            |current| form::without_sequence(current, 0),
            |default| form::default_without(default, 0),
        );
        assert_eq!(refused, Err(form::SEQUENCE_CHANGED.to_owned()));
        assert_eq!(bytes(), before);

        // The same list is no change, and writes nothing.
        let same = apply_sequences(&path, Some((0, &hello())), <[_]>::to_vec, |d| d);
        assert_eq!(same, Ok(None));
        assert_eq!(bytes(), before);

        // Move Down takes the default along.
        seed(&path, two_with_default(Some(0)));
        let moved = apply_sequences(
            &path,
            Some((0, &hello())),
            |current| form::moved(current, 0, Move::Down),
            |default| form::moved_default(default, 0, Move::Down),
        );
        assert_eq!(moved, Ok(Some(2)));
        let local = settings::load(&path).local_terminal;
        assert_eq!(local.sequences, [second(), hello()]);
        assert_eq!(local.startup_sequence, Some(1), "still on Hello");

        // Deleting the default clears it.
        seed(&path, two_with_default(Some(0)));
        let deleted = apply_sequences(
            &path,
            Some((0, &hello())),
            |current| form::without_sequence(current, 0),
            |default| form::default_without(default, 0),
        );
        assert_eq!(deleted, Ok(Some(1)));
        let local = settings::load(&path).local_terminal;
        assert_eq!(local.sequences, [second()]);
        assert_eq!(local.startup_sequence, None);

        // Deleting one before the default moves the default up with its own.
        seed(&path, two_with_default(Some(1)));
        apply_sequences(
            &path,
            Some((0, &hello())),
            |current| form::without_sequence(current, 0),
            |default| form::default_without(default, 0),
        )
        .unwrap();
        let local = settings::load(&path).local_terminal;
        assert_eq!(local.startup_sequence, Some(0));
        assert_eq!(local.default_sequence(), Some(&second()));

        // A default that names no sequence is dropped, not saved.
        seed(&path, two_with_default(Some(0)));
        apply_sequences(
            &path,
            Some((1, &second())),
            |current| form::with_sequence(current, Some(1), seq("Second", &["df"])),
            |_| Some(9),
        )
        .unwrap();
        assert_eq!(settings::load(&path).local_terminal.startup_sequence, None);
    }

    #[test]
    fn a_default_past_the_end_is_not_brought_to_life_by_a_longer_list() {
        let (_dir, path) = world();
        seed(
            &path,
            LocalTerminal {
                sequences: vec![hello()],
                startup_sequence: Some(1),
                transcript: false,
            },
        );
        apply_sequences(
            &path,
            None,
            |current| form::with_sequence(current, None, second()),
            |default| default,
        )
        .unwrap();
        let local = settings::load(&path).local_terminal;
        assert_eq!(local.sequences, [hello(), second()]);
        assert_eq!(local.startup_sequence, None, "it named nothing before");
    }

    #[test]
    fn a_sequence_the_reader_would_skip_is_refused() {
        let (_dir, path) = world();
        seed(&path, two_with_default(Some(0)));
        let before = fs::read(&path).unwrap();
        for bad in [
            seq("  ", &["ls"]),
            seq("a\u{1b}b", &["ls"]),
            seq("ok", &["a\nb"]),
            seq("ok", &["a\0b"]),
        ] {
            let refused = apply_sequences(
                &path,
                None,
                |current| form::with_sequence(current, None, bad.clone()),
                |default| default,
            );
            assert!(refused.is_err(), "{bad:?}");
            assert_eq!(fs::read(&path).unwrap(), before, "{bad:?}: nothing written");
        }
    }

    #[test]
    fn set_default_names_its_sequence() {
        let (_dir, path) = world();
        seed(&path, two_with_default(None));
        assert_eq!(set_default(&path, Some((1, second()))), Ok(()));
        assert_eq!(
            settings::load(&path).local_terminal.startup_sequence,
            Some(1)
        );

        // The pick means a sequence the file no longer has at that place.
        let before = fs::read(&path).unwrap();
        assert_eq!(
            set_default(&path, Some((1, hello()))),
            Err(form::CHOICE_CHANGED.to_owned())
        );
        assert_eq!(
            set_default(&path, Some((5, hello()))),
            Err(form::CHOICE_CHANGED.to_owned())
        );
        assert_eq!(fs::read(&path).unwrap(), before);

        assert_eq!(set_default(&path, None), Ok(()));
        assert_eq!(settings::load(&path).local_terminal.startup_sequence, None);
    }

    #[test]
    fn set_transcript_keeps_the_rest() {
        let (_dir, path) = world();
        settings::save(
            &path,
            &GuiSettings {
                font_scale: 1.5,
                local_terminal: two_with_default(Some(1)),
                ..GuiSettings::default()
            },
        )
        .unwrap();
        assert_eq!(set_transcript(&path, true), Ok(()));
        let loaded = settings::load(&path);
        assert!(loaded.local_terminal.transcript);
        assert_eq!(loaded.local_terminal.sequences, [hello(), second()]);
        assert_eq!(loaded.local_terminal.startup_sequence, Some(1));
        assert_eq!(loaded.font_scale, 1.5);

        assert_eq!(set_transcript(&path, false), Ok(()));
        assert!(!settings::load(&path).local_terminal.transcript);
    }

    #[test]
    fn a_missing_file_is_made_by_the_first_change_and_not_by_a_nothing() {
        let (_dir, path) = world();
        // Off on a missing file changes nothing, so it creates nothing.
        assert_eq!(set_transcript(&path, false), Ok(()));
        assert_eq!(set_default(&path, None), Ok(()));
        assert!(!path.exists());
        assert_eq!(set_transcript(&path, true), Ok(()));
        assert!(settings::load(&path).local_terminal.transcript);
    }

    #[test]
    fn a_damaged_file_is_never_written() {
        let (_dir, path) = world();
        fs::write(&path, "not json").unwrap();
        let results = [
            apply_sequences(
                &path,
                None,
                |current| form::with_sequence(current, None, hello()),
                |default| default,
            )
            .map(|_| ()),
            set_default(&path, None),
            set_transcript(&path, true),
        ];
        for result in results {
            let err = result.unwrap_err();
            assert!(
                err.starts_with("Could not save the local terminal: "),
                "{err}"
            );
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), "not json");
    }
}
