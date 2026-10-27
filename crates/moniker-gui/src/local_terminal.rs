//! Local terminal's own startup sequences, default sequence and "Save
//! transcript" switch (issue #13): the data, its file and its edits. Nothing
//! here touches GTK.
//!
//! A profile keeps its sequences in `profiles.json`, which syncs. Local
//! terminal's must never leave this machine, or any machine holding the sync
//! key could put commands into this one's shells, so they are not a profile
//! and not in that file. They are in a file of their own beside it,
//! `local-terminal.json` ([`FILE_NAME`]): this machine's own, mode 0600 in a
//! folder of mode 0700, written whole and atomically
//! ([`settings::write_private`]), never synced and unknown to `moniker-server`,
//! as pretty JSON:
//! `{"version": 1, "sequences": [...], "startup_sequence": N, "transcript": bool}`,
//! with no `startup_sequence` while there is no default.
//!
//! Why a file of its own and not a field of the GUI settings file: Moniker
//! 0.6.0 and older read `gui-settings.json` field by field and write the whole
//! of what they read back at every change (a window resized, the text size at
//! quit, a Preferences switch), so any older Moniker started on this machine
//! (the installed one, a downgrade, the one Files' "Open in Terminal" starts)
//! would drop the sequences without a word. A file they never knew they never
//! touch.
//!
//! Reading ([`load`]): a missing file is the default (no sequence, no default,
//! the switch off), what a machine that never used any of this has. A file that
//! is a JSON object with `"version": 1` is read field by field: a missing, null
//! or odd field is that field's default and costs the others nothing. A
//! sequence that breaks a profile's rules ([`StartupSequence::check`]) or is
//! not of the right shape is skipped, which only a hand edit can cause; the
//! default is a place in the list, so it stays on the sequence the file named
//! while the entries before it are skipped, and is none if its own was. The
//! default is an index, and one past the end of the list names nothing
//! ([`LocalTerminal::startup_choice`]), as a profile's does. Anything else (a
//! file that cannot be read, is not JSON, is not an object, or has another
//! version or none, a newer Moniker's say) is unusable: running treats it as
//! the default, with a line on stderr naming the file, and every change is
//! refused and leaves it as it is ([`update`]), so a file the person may want
//! to repair, or that a newer Moniker wrote, is never replaced. What is
//! skipped goes at the next write.
//!
//! The edits mirror a profile's ([`crate::form::apply_sequences`] and the
//! default's pick): the change is built from the list the file holds when it
//! is saved, only if the sequence at its place is still the one the page was
//! opened on, and a pick of the default carries the sequence it means. Both
//! are refused otherwise ([`LOCAL_SEQUENCE_CHANGED`],
//! [`form::CHOICE_CHANGED`]), and nothing is written. No message quotes a
//! command.

use std::fs;
use std::io;
use std::path::Path;

use moniker_core::profile::StartupSequence;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::form;
use crate::settings;

/// The file's name, beside `profiles.json` and `gui-settings.json`.
pub const FILE_NAME: &str = "local-terminal.json";

/// The format version this build reads and writes.
const VERSION: u64 = 1;

/// What the page for Local terminal says about what its sequences and
/// transcripts may hold. A profile's page says the same about its own.
pub const LOCAL_SECRETS_WARNING: &str = "Startup commands are saved as plain text in Moniker's settings file, and a transcript records whatever appears in the terminal, including anything you type or paste. Keep passwords out of both.";

/// The description of the Startup sequences group on Local terminal's page:
/// where they run, that they are never synced, and [`LOCAL_SECRETS_WARNING`].
pub const LOCAL_SEQUENCES_NOTE: &str = "These run in a shell on this computer when you open Local terminal, and are never synced. Startup commands are saved as plain text in Moniker's settings file, and a transcript records whatever appears in the terminal, including anything you type or paste. Keep passwords out of both.";

/// What Local terminal's sequence sub-page says when the sequence it was opened
/// on is not what the file holds any more: [`form::SEQUENCE_CHANGED`] without
/// its "perhaps by a sync", as Local terminal's never sync.
pub const LOCAL_SEQUENCE_CHANGED: &str = "This startup sequence was changed or removed elsewhere. \
     Go back to the list to see what is saved now.";

/// Local terminal's sequences, default and transcript switch, as its file
/// holds them. The default value (no sequences, no default, transcript off) is
/// what a machine that never used any of this has: no file.
///
/// `Debug` prints every command, and commands can hold secrets: do not log it.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct LocalTerminal {
    /// Every sequence is one [`StartupSequence::check`] accepts.
    pub sequences: Vec<StartupSequence>,
    /// The sequence a plain pick of Local terminal runs, as an index into
    /// `sequences`. May point past the end: see [`LocalTerminal::startup_choice`].
    pub startup_sequence: Option<usize>,
    /// Save a transcript of every local tab (the switch on Local terminal's
    /// page; "Save transcripts of all sessions" also does).
    pub transcript: bool,
}

/// What the file holds, as it is written: the version first.
#[derive(Serialize)]
struct Written<'a> {
    version: u64,
    sequences: &'a [StartupSequence],
    #[serde(skip_serializing_if = "Option::is_none")]
    startup_sequence: Option<usize>,
    transcript: bool,
}

impl LocalTerminal {
    /// The local terminal in a version 1 file's `fields`. Each of its three
    /// fields is read on its own and falls back to its default: a missing one,
    /// a null, or one of the wrong type costs the others nothing.
    fn from_fields(fields: &Map<String, Value>) -> LocalTerminal {
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
}

/// An unusable file's reason, as an error of the kind [`read`] gives.
fn unusable(why: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why.into())
}

/// Local terminal's file at `path`: `None` if there is none, what it holds
/// otherwise, and an error if it exists but cannot be used (module docs): it
/// cannot be read, is not JSON (text that is not UTF-8 included), is not a JSON
/// object, or is not version 1. The error says why and never what the file
/// holds: a syntax error says where, a version says its number.
fn read(path: &Path) -> io::Result<Option<LocalTerminal>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let value: Value =
        serde_json::from_str(&text).map_err(|err| unusable(format!("it is not JSON ({err})")))?;
    let fields = value
        .as_object()
        .ok_or_else(|| unusable("it is not a JSON object"))?;
    match fields.get("version") {
        Some(Value::Number(n)) if n.as_u64() == Some(VERSION) => {}
        Some(Value::Number(n)) => {
            return Err(unusable(format!(
                "it is version {n}; this Moniker reads version {VERSION}"
            )));
        }
        _ => {
            return Err(unusable(format!(
                "it has no version number; this Moniker reads version {VERSION}"
            )));
        }
    }
    Ok(Some(LocalTerminal::from_fields(fields)))
}

/// Local terminal as the file at `path` has it. A missing file is the default;
/// a file that cannot be used is the default too, and stderr says which file
/// it was and why (module docs).
pub fn load(path: &Path) -> LocalTerminal {
    match read(path) {
        Ok(local) => local.unwrap_or_default(),
        Err(err) => {
            eprintln!(
                "moniker: cannot use {}, so Local terminal has no startup sequences: {err}",
                path.display()
            );
            LocalTerminal::default()
        }
    }
}

/// Write `local` to `path` ([`settings::write_private`]: mode 0600, atomic) as
/// pretty JSON with a trailing newline, the version first.
pub(crate) fn save(path: &Path, local: &LocalTerminal) -> io::Result<()> {
    let written = Written {
        version: VERSION,
        sequences: &local.sequences,
        startup_sequence: local.startup_sequence,
        transcript: local.transcript,
    };
    let mut bytes = serde_json::to_vec_pretty(&written).map_err(io::Error::other)?;
    bytes.push(b'\n');
    settings::write_private(path, &bytes)
}

/// Change Local terminal's file at `path` with `change`: read it now, change
/// what it holds, write it ([`save`]). Every change to it goes through here.
///
/// A missing file starts from the default, and nothing is written if `change`
/// leaves it as it was (so a change that changes nothing makes no file).
/// Nothing is written, with an error, if the file exists but cannot be used
/// (module docs), and `change` is not called: such a file is left as it is,
/// not replaced by the default plus one change, and a permission problem that
/// hides it is never mistaken for "no sequences yet".
///
/// This is a read and a rename, not a lock, as the settings file's
/// [`settings::update`] is: the app makes its changes from its one main thread,
/// one at a time, and two Monikers changing it at the same moment can lose one
/// change.
pub(crate) fn update(path: &Path, change: impl FnOnce(&mut LocalTerminal)) -> io::Result<()> {
    let before = read(path)
        .map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("the file is unusable, so it is left as it is: {err}"),
            )
        })?
        .unwrap_or_default();
    let mut after = before.clone();
    change(&mut after);
    if after == before {
        return Ok(());
    }
    save(path, &after)
}

/// What a failed save says, whatever failed: the file could not be read, used
/// or written ([`update`]).
fn save_error(err: &io::Error) -> String {
    format!("Could not save the local terminal: {err}")
}

/// Change Local terminal's sequences in its file at `path` as the file holds
/// them now: `change` gets the list and returns the one to save.
///
/// The sub-page works on one sequence, opened at `target` (its index and what
/// it held then). The file can change while the page is open (another window,
/// a hand edit), and the index could then name a different sequence: moving or
/// deleting that one would be a silent loss, and saving a list built from an
/// older one would undo the edit. So the check and the change are made on what
/// the file holds: the sequence at that index must still be the one the page
/// was opened on, else [`LOCAL_SEQUENCE_CHANGED`] and nothing is written. A new
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
/// the file cannot be used or written (one that exists but cannot be used is
/// left as it is).
pub fn apply_sequences(
    path: &Path,
    target: Option<(usize, &StartupSequence)>,
    change: impl FnOnce(&[StartupSequence]) -> Vec<StartupSequence>,
    follow: impl FnOnce(Option<usize>) -> Option<usize>,
) -> Result<Option<usize>, String> {
    let mut refusal = None;
    let mut written = None;
    update(path, |local| {
        if let Some((index, expected)) = target
            && local.sequences.get(index) != Some(expected)
        {
            refusal = Some(LOCAL_SEQUENCE_CHANGED.to_owned());
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
/// (`None`: always allowed on a usable file, and it also clears a default that
/// named nothing), in its file at `path`.
///
/// A pick carries the sequence it means as well as its place in the list (the
/// page's view of both), and is refused with [`form::CHOICE_CHANGED`], writing
/// nothing, unless the file has that very sequence (name and commands) at that
/// place now: the default is kept by position, so a list changed since the
/// page showed it would otherwise make the position name another sequence,
/// and a pick must never run a sequence the person did not choose. The message
/// quotes no command.
pub fn set_default(path: &Path, choice: Option<(usize, StartupSequence)>) -> Result<(), String> {
    let mut refusal = None;
    update(path, |local| match &choice {
        None => local.startup_sequence = None,
        Some((index, expected)) if local.sequences.get(*index) == Some(expected) => {
            local.startup_sequence = Some(*index);
        }
        Some(_) => refusal = Some(form::CHOICE_CHANGED.to_owned()),
    })
    .map_err(|err| save_error(&err))?;
    match refusal {
        Some(refusal) => Err(refusal),
        None => Ok(()),
    }
}

/// Turn Local terminal's own "Save transcript" switch on or off in its file at
/// `path`, keeping its sequences and default as the file has them.
pub fn set_transcript(path: &Path, on: bool) -> Result<(), String> {
    update(path, |local| local.transcript = on).map_err(|err| save_error(&err))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::form::{self, Move};
    use crate::settings;
    use serde_json::json;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
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

    /// Local terminal's file in a fresh directory (nothing written yet).
    fn world() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        (dir, path)
    }

    /// Save `local` as Local terminal's file at `path`.
    fn seed(path: &Path, local: LocalTerminal) {
        save(path, &local).unwrap();
    }

    fn two_with_default(default: Option<usize>) -> LocalTerminal {
        LocalTerminal {
            sequences: vec![hello(), second()],
            startup_sequence: default,
            transcript: false,
        }
    }

    /// What Moniker reads from a version 1 file that holds `fields` (an
    /// object) besides its version.
    fn read_back(fields: Value) -> LocalTerminal {
        let (_dir, path) = world();
        let mut fields = fields.as_object().expect("an object").clone();
        fields.insert("version".to_owned(), json!(1));
        fs::write(&path, Value::Object(fields).to_string()).unwrap();
        read(&path).unwrap().expect("a file")
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn is_root() -> bool {
        rustix::process::geteuid().is_root()
    }

    #[test]
    fn a_missing_file_is_the_default() {
        let (_dir, path) = world();
        assert!(matches!(read(&path), Ok(None)));
        assert_eq!(load(&path), LocalTerminal::default());
        assert_eq!(read_back(json!({})), LocalTerminal::default());
    }

    #[test]
    fn each_field_falls_back_on_its_own() {
        let local = read_back(json!({"sequences": "x", "startup_sequence": 0, "transcript": true}));
        assert!(local.sequences.is_empty());
        assert_eq!(local.startup_sequence, Some(0));
        assert_eq!(local.startup_choice(), None, "nothing for it to name");
        assert!(local.transcript);

        for bad in [json!(-1), json!(1.5), json!("0"), json!(null), json!(true)] {
            let local = read_back(
                json!({"sequences": [hello()], "startup_sequence": bad, "transcript": true}),
            );
            assert_eq!(local.startup_sequence, None, "{bad}");
            assert_eq!(local.sequences, [hello()], "{bad}: the others stay");
            assert!(local.transcript, "{bad}: the others stay");
        }
        for bad in [json!("yes"), json!(1), json!(null), json!([])] {
            let local = read_back(
                json!({"sequences": [hello()], "startup_sequence": 0, "transcript": bad}),
            );
            assert!(!local.transcript, "{bad}");
            assert_eq!(local.sequences, [hello()], "{bad}: the others stay");
            assert_eq!(local.startup_sequence, Some(0), "{bad}: the others stay");
        }
    }

    #[test]
    fn entries_it_cannot_use_are_skipped() {
        let local = read_back(json!({"sequences": [
            hello(),
            {"name": "", "commands": ["a"]},
            {"name": "a\u{1b}b", "commands": ["a"]},
            {"name": "x", "commands": ["a\nb"]},
            {"name": "x", "commands": ["a\u{0}b"]},
            {"name": "y", "commands": [1]},
            {"name": "z"},
            5,
        ]}));
        assert_eq!(local.sequences, [hello()]);
    }

    #[test]
    fn a_skipped_entry_does_not_move_the_default_to_another_sequence() {
        // The default is a place in the list, and a list with an entry taken
        // out has its places changed: the default stays on the sequence the
        // file named, or on none if that one was taken out.
        let bad = json!({"name": "", "commands": ["a"]});
        let read = |default: u64| {
            read_back(json!({"sequences": [hello(), bad, second()], "startup_sequence": default}))
        };
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
        let local = read_back(json!({"sequences": [hello(), second()], "startup_sequence": 3}));
        assert_eq!(local.startup_choice(), None);
        assert_eq!(local.default_sequence(), None);
        let local = read_back(json!({"sequences": [hello(), second()], "startup_sequence": 1}));
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
    fn a_changed_sequence_is_told_without_a_sync() {
        assert_eq!(
            LOCAL_SEQUENCE_CHANGED,
            "This startup sequence was changed or removed elsewhere. \
             Go back to the list to see what is saved now."
        );
        assert!(!LOCAL_SEQUENCE_CHANGED.contains("sync"));
        assert!(form::SEQUENCE_CHANGED.contains("perhaps by a sync"));
    }

    #[test]
    fn only_version_1_is_read() {
        let (_dir, path) = world();
        let with = |version: Option<Value>| {
            let mut file =
                json!({"sequences": [hello()], "startup_sequence": 0, "transcript": true});
            if let Some(version) = version {
                file["version"] = version;
            }
            fs::write(&path, file.to_string()).unwrap();
        };
        with(Some(json!(1)));
        assert_eq!(
            load(&path),
            LocalTerminal {
                sequences: vec![hello()],
                startup_sequence: Some(0),
                transcript: true,
            }
        );

        const NONE: &str = "it has no version number; this Moniker reads version 1";
        let refused = [
            (None, NONE),
            (Some(json!(null)), NONE),
            (Some(json!("1")), NONE),
            (Some(json!(true)), NONE),
            (Some(json!([1])), NONE),
            (
                Some(json!(2)),
                "it is version 2; this Moniker reads version 1",
            ),
            (
                Some(json!(0)),
                "it is version 0; this Moniker reads version 1",
            ),
            (
                Some(json!(-1)),
                "it is version -1; this Moniker reads version 1",
            ),
            (
                Some(json!(1.5)),
                "it is version 1.5; this Moniker reads version 1",
            ),
        ];
        for (version, why) in refused {
            with(version.clone());
            let err = read(&path).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{version:?}");
            assert_eq!(err.to_string(), why, "{version:?}");
            assert_eq!(
                load(&path),
                LocalTerminal::default(),
                "{version:?}: runs as none"
            );
        }
    }

    #[test]
    fn a_file_it_cannot_read_runs_as_the_default() {
        let (dir, path) = world();
        let bad: [&[u8]; 7] = [
            b"",
            b"not json",
            b"[1, 2]",
            b"\"x\"",
            b"null",
            b"{\"version\": 1",
            b"\xff\xfe",
        ];
        for bad in bad {
            fs::write(&path, bad).unwrap();
            assert!(read(&path).is_err(), "{bad:?}");
            assert_eq!(load(&path), LocalTerminal::default(), "{bad:?}");
        }
        // A directory where the file should be cannot be read.
        let as_dir = dir.path().join("dir");
        fs::create_dir(&as_dir).unwrap();
        assert!(read(&as_dir).is_err());
        assert_eq!(load(&as_dir), LocalTerminal::default());
    }

    #[test]
    fn an_unreadable_file_refuses_every_change_and_keeps_its_bytes() {
        let (dir, path) = world();
        // What a version this build does not read may hold: a command, which
        // no message may quote.
        let newer = json!({
            "version": 2,
            "sequences": [{"name": "S", "commands": ["secret-command"]}],
        })
        .to_string();
        // Each file's bytes, and whether this process is kept from reading it
        // (root reads it anyway, so that one is left out as root).
        let mut damaged: Vec<(Vec<u8>, bool)> = vec![
            (b"not json".to_vec(), false),
            (b"[1]".to_vec(), false),
            (b"{\"sequences\": []}".to_vec(), false),
            (b"{\"version\": \"1\"}".to_vec(), false),
            (b"\xff\xfe".to_vec(), false),
            (newer.into_bytes(), false),
        ];
        if !is_root() {
            damaged.push((b"{\"version\": 1}".to_vec(), true));
        }
        for (n, (bytes, locked)) in damaged.iter().enumerate() {
            fs::write(&path, bytes).unwrap();
            if *locked {
                fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
            }
            let results = [
                apply_sequences(
                    &path,
                    None,
                    |current| form::with_sequence(current, None, hello()),
                    |default| default,
                )
                .map(|_| ()),
                apply_sequences(
                    &path,
                    Some((0, &hello())),
                    |current| form::without_sequence(current, 0),
                    |default| form::default_without(default, 0),
                )
                .map(|_| ()),
                set_default(&path, None),
                set_default(&path, Some((0, hello()))),
                set_transcript(&path, true),
                set_transcript(&path, false),
            ];
            for result in results {
                let err = result.unwrap_err();
                assert!(
                    err.starts_with(
                        "Could not save the local terminal: the file is unusable, \
                         so it is left as it is: "
                    ),
                    "{n}: {err}"
                );
                assert!(!err.contains("secret-command"), "{n}: {err}");
            }
            if *locked {
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            }
            assert_eq!(&fs::read(&path).unwrap(), bytes, "{n}: left as it was");
            let names: Vec<_> = fs::read_dir(dir.path())
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert_eq!(names, [FILE_NAME], "{n}: no temporary file left");
        }

        // A directory in the file's place is refused too, and left.
        let as_dir = dir.path().join("dir");
        fs::create_dir(&as_dir).unwrap();
        assert!(set_transcript(&as_dir, true).is_err());
        assert!(as_dir.is_dir());
    }

    #[test]
    fn it_is_written_with_mode_0600_in_a_private_folder() {
        let dir = tempfile::tempdir().unwrap();
        // The config directory may not exist yet: nothing was ever saved.
        let path = dir.path().join("cfg").join("moniker").join(FILE_NAME);
        assert_eq!(set_transcript(&path, true), Ok(()));
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{\n  \"version\": 1,\n  \"sequences\": [],\n  \"transcript\": true\n}\n"
        );

        // Every write replaces it the same way (still 0600, nothing left).
        apply_sequences(
            &path,
            None,
            |current| form::with_sequence(current, None, hello()),
            |default| default,
        )
        .unwrap();
        set_default(&path, Some((0, hello()))).unwrap();
        assert_eq!(mode(&path), 0o600);
        let names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [FILE_NAME], "only the file is left");
    }

    #[test]
    fn it_round_trips_as_pretty_json_with_its_version() {
        let (_dir, path) = world();
        let local = LocalTerminal {
            sequences: vec![hello()],
            startup_sequence: Some(0),
            transcript: true,
        };
        seed(&path, local.clone());
        assert_eq!(load(&path), local);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            r#"{
  "version": 1,
  "sequences": [
    {
      "name": "Hello",
      "commands": [
        "printf hi"
      ]
    }
  ],
  "startup_sequence": 0,
  "transcript": true
}
"#
        );

        // No default sequence is no key, not a null; the defaults may be
        // written.
        seed(&path, LocalTerminal::default());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{\n  \"version\": 1,\n  \"sequences\": [],\n  \"transcript\": false\n}\n"
        );
        assert_eq!(load(&path), LocalTerminal::default());
    }

    #[test]
    fn a_sequence_it_cannot_read_goes_at_the_next_write() {
        let (_dir, path) = world();
        let file = json!({
            "version": 1,
            "sequences": [hello(), {"name": "", "commands": ["hand-edited"]}],
            "startup_sequence": 0,
        });
        fs::write(&path, file.to_string()).unwrap();
        // Reading it writes nothing.
        assert_eq!(load(&path).sequences, [hello()]);
        assert!(fs::read_to_string(&path).unwrap().contains("hand-edited"));
        set_transcript(&path, true).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("hand-edited"), "{text}");
        let local = load(&path);
        assert_eq!(local.sequences, [hello()]);
        assert_eq!(local.startup_sequence, Some(0));
        assert!(local.transcript);
    }

    #[test]
    fn settings_writes_never_touch_it() {
        let (dir, path) = world();
        let settings_path = dir.path().join(settings::FILE_NAME);
        let local = LocalTerminal {
            sequences: vec![hello(), second()],
            startup_sequence: Some(1),
            transcript: true,
        };
        seed(&path, local.clone());
        let bytes = fs::read(&path).unwrap();
        let inode = fs::metadata(&path).unwrap().ino();
        let untouched = |what: &str| {
            assert_eq!(fs::read(&path).unwrap(), bytes, "{what}");
            assert_eq!(
                fs::metadata(&path).unwrap().ino(),
                inode,
                "{what}: not replaced"
            );
            assert_eq!(load(&path), local, "{what}");
        };

        settings::save_font_scale(&settings_path, 1.5).unwrap();
        assert_eq!(settings::load(&settings_path).font_scale, 1.5);
        untouched("text size");

        settings::update(&settings_path, |s| s.ping_hosts = false).unwrap();
        assert!(!settings::load(&settings_path).ping_hosts);
        untouched("a switch");

        settings::save(&settings_path, &settings::GuiSettings::default()).unwrap();
        untouched("the whole settings file");

        let text = fs::read_to_string(&settings_path).unwrap();
        assert!(
            !text.contains("Hello") && !text.contains("local_terminal"),
            "{text}"
        );
    }

    #[test]
    fn the_old_field_in_the_settings_file_is_ignored() {
        // Earlier builds of this change kept Local terminal in the settings
        // file. None was released, so it is not moved over: it is left unread,
        // and goes at the settings file's next write.
        let (dir, path) = world();
        let settings_path = dir.path().join(settings::FILE_NAME);
        let old = json!({
            "font_scale": 1.5,
            "local_terminal": {
                "sequences": [hello()],
                "startup_sequence": 0,
                "transcript": true,
            },
        });
        fs::write(&settings_path, old.to_string()).unwrap();
        assert_eq!(load(&path), LocalTerminal::default());
        assert!(!path.exists(), "nothing moved over");
        assert_eq!(
            settings::load(&settings_path).font_scale,
            1.5,
            "the rest is read"
        );

        settings::update(&settings_path, |s| s.ping_hosts = false).unwrap();
        assert!(!path.exists());
        let text = fs::read_to_string(&settings_path).unwrap();
        assert!(!text.contains("local_terminal"), "{text}");
        assert_eq!(settings::load(&settings_path).font_scale, 1.5);
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
        assert_eq!(load(&path).sequences, [hello()]);

        // The page was opened on another sequence than the file holds.
        let before = bytes();
        let other = seq("Other", &["ls"]);
        let refused = apply_sequences(
            &path,
            Some((0, &other)),
            |current| form::without_sequence(current, 0),
            |default| form::default_without(default, 0),
        );
        assert_eq!(refused, Err(LOCAL_SEQUENCE_CHANGED.to_owned()));
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
        let local = load(&path);
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
        let local = load(&path);
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
        let local = load(&path);
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
        assert_eq!(load(&path).startup_sequence, None);

        // On a missing file there is no sequence the page could have been
        // opened on.
        let (_dir, missing) = world();
        let refused = apply_sequences(
            &missing,
            Some((0, &hello())),
            |current| form::without_sequence(current, 0),
            |default| default,
        );
        assert_eq!(refused, Err(LOCAL_SEQUENCE_CHANGED.to_owned()));
        assert!(!missing.exists(), "nothing written");
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
        let local = load(&path);
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
        assert_eq!(load(&path).startup_sequence, Some(1));

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
        assert_eq!(load(&path).startup_sequence, None);

        // None also clears a default that names nothing (a hand edit).
        seed(&path, two_with_default(Some(7)));
        assert_eq!(set_default(&path, None), Ok(()));
        assert_eq!(load(&path).startup_sequence, None);
    }

    #[test]
    fn set_transcript_keeps_the_rest() {
        let (_dir, path) = world();
        seed(&path, two_with_default(Some(1)));
        assert_eq!(set_transcript(&path, true), Ok(()));
        let loaded = load(&path);
        assert!(loaded.transcript);
        assert_eq!(loaded.sequences, [hello(), second()]);
        assert_eq!(loaded.startup_sequence, Some(1));

        assert_eq!(set_transcript(&path, false), Ok(()));
        assert!(!load(&path).transcript);
    }

    #[test]
    fn a_missing_file_is_made_by_the_first_change_and_not_by_a_nothing() {
        let (_dir, path) = world();
        // Off on a missing file changes nothing, so it creates nothing.
        assert_eq!(set_transcript(&path, false), Ok(()));
        assert_eq!(set_default(&path, None), Ok(()));
        assert!(!path.exists());
        assert_eq!(set_transcript(&path, true), Ok(()));
        assert!(load(&path).transcript);
    }
}
