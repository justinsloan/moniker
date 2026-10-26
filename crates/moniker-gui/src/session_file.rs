//! Saved sessions: the layouts of windows, kept in `sessions.json` beside
//! `gui-settings.json` (issue #12).
//!
//! A session is a name and a list of tabs; a tab is a tree of splits with a
//! pane at each leaf. Per pane the file says what it was (a profile by id, with
//! its name only to say which profile is gone; a local terminal and the folder
//! its shell reported; or the new-tab list), the name the person gave it, its
//! text size and whether it saved a transcript. The types are the whole of what
//! can be written, so the file never holds a password, a host, a user name, a
//! command line, a sequence, an environment or any text from a terminal.
//!
//! Like the settings, the file is this machine's own: never synced, mode 0600,
//! written atomically ([`crate::settings::write_private`]). It is
//! `{"version":1,"sessions":[...]}` and is read field by field: an entry that
//! makes no sense is skipped and costs the others nothing; a file that is not
//! JSON, is of another version or is not a list of sessions is
//! [`Loaded::Unreadable`] and is never written over unless the caller says so.
//! A skipped entry is not kept: the next write leaves it out.
//!
//! The reading limits keep a damaged or hand-made file harmless: a tab nests at
//! most [`MAX_SPLIT_DEPTH`] splits deep and holds at most [`MAX_PANES`] panes
//! (work stops at the first pane or split past either), and serde_json itself
//! refuses a file nested past its own limit before any of this runs.
//!
//! This module is pure: no GTK, no display. Every write reads the file again
//! first, so a session saved meanwhile (from another window) is kept.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::settings::write_private;
use crate::zoom;

/// The file's name, beside `gui-settings.json`.
// Used first by the sessions menu (task 5), which finds the file beside the
// settings.
#[allow(dead_code)]
pub(crate) const FILE_NAME: &str = "sessions.json";

/// The format version this build reads and writes.
const VERSION: u64 = 1;
/// The most splits one inside the other in a tab; a tab nested deeper is
/// skipped.
const MAX_SPLIT_DEPTH: usize = 16;
/// The most panes in a tab; a tab with more is skipped.
const MAX_PANES: usize = 32;
/// The longest profile id kept, in characters (a canonical id has 36).
const MAX_ID_CHARS: usize = 64;
/// The longest session name, in characters ([`check_name`]).
const MAX_NAME_CHARS: usize = 100;
/// The longest pane name or profile name kept, in characters.
const MAX_LABEL_CHARS: usize = 200;
/// A split's divider, as a fraction of the split, stays within these ends and
/// is in the middle when the file gives none.
const FRACTION_RANGE: (f64, f64) = (0.05, 0.95);
const FRACTION_DEFAULT: f64 = 0.5;

/// A saved session: a named layout of a window.
// Used first by `session_layout` (capture and restore, task 4) and the
// sessions menu (task 5).
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Session {
    /// Passes [`check_name`] and is unique in the file.
    pub(crate) name: String,
    /// In tab order; never empty.
    pub(crate) tabs: Vec<TabRecord>,
}

/// One tab of a session.
// Used first by `session_layout` (task 4).
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TabRecord {
    pub(crate) root: Node,
    /// The reading-order index (start before end, depth first) of the pane that
    /// had the focus; always one of the tab's panes.
    pub(crate) focused: usize,
}

/// A tab's split tree.
// Used first by `session_layout` (task 4).
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Node {
    Pane(PaneRecord),
    Split {
        /// Side by side (a horizontal `gtk::Paned`) rather than one above the
        /// other.
        horizontal: bool,
        /// Where the divider was, as a fraction of the split's size.
        fraction: f64,
        start: Box<Node>,
        end: Box<Node>,
    },
}

/// One pane.
// Used first by `session_layout` (task 4).
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PaneRecord {
    pub(crate) kind: Kind,
    /// The name the person gave the pane, if any.
    pub(crate) name: Option<String>,
    /// The text size, within `zoom::MIN..=zoom::MAX`.
    pub(crate) scale: f64,
    /// Whether the pane saved a transcript.
    pub(crate) transcript: bool,
}

/// What a pane was.
// Used first by `session_layout` (task 4).
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Kind {
    /// A connection to a saved profile, by id. `name` is the profile's name at
    /// the time, kept only to say which profile is gone.
    Profile { id: String, name: String },
    /// A local terminal and the folder its shell last reported (an absolute
    /// path), if it did.
    Local { dir: Option<PathBuf> },
    /// The new-tab list.
    List,
}

/// What reading the file found.
// Used first by the sessions menu (task 5).
#[allow(dead_code)]
#[derive(Debug, PartialEq)]
pub(crate) enum Loaded {
    /// The sessions in the file, in file order; none for a missing file.
    Sessions(Vec<Session>),
    /// The file exists but cannot be used. The text is short and plain, fits
    /// after "Could not read the saved sessions: ", and never holds anything
    /// the file said.
    Unreadable(String),
}

/// The saved sessions at `path`.
// Used first by the sessions menu (task 5).
#[allow(dead_code)]
pub(crate) fn load(path: &Path) -> Loaded {
    match read(path) {
        Ok(sessions) => Loaded::Sessions(sessions),
        Err(why) => Loaded::Unreadable(why),
    }
}

/// `raw` as a session name: trimmed, and refused with the text to show if it
/// is empty, holds a control character or is over 100 characters.
// Used first by the sessions menu (task 5).
#[allow(dead_code)]
pub(crate) fn check_name(raw: &str) -> Result<String, &'static str> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("Enter a name.");
    }
    if name.chars().any(char::is_control) {
        return Err("A name cannot contain control characters.");
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err("A name can be at most 100 characters.");
    }
    Ok(name.to_owned())
}

/// Save `session` in the file at `path`: read the file again, replace the
/// session of the same name (keeping its place) or add this one at the end,
/// and write the file.
///
/// A file that cannot be read is an error and is left as it is, unless
/// `replace_unreadable` says the person was told and chose to start over: then
/// the file is replaced by one holding only `session`. A session that reading
/// would skip (a name that fails [`check_name`], too many panes or splits, an
/// id that is too long) is an error too, so what is saved is always found
/// again.
// Used first by the sessions menu (task 5).
#[allow(dead_code)]
pub(crate) fn put(path: &Path, session: Session, replace_unreadable: bool) -> io::Result<()> {
    if !is_storable(&session) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "the session cannot be saved: a name or an id is not allowed, or a tab \
                 has more than {MAX_PANES} panes or splits more than {MAX_SPLIT_DEPTH} deep"
            ),
        ));
    }
    let mut sessions = match read(path) {
        Ok(sessions) => sessions,
        Err(_) if replace_unreadable => Vec::new(),
        Err(why) => return Err(unreadable_error(&why)),
    };
    match sessions.iter_mut().find(|saved| saved.name == session.name) {
        Some(saved) => *saved = session,
        None => sessions.push(session),
    }
    write(path, &sessions)
}

/// Rename the session `from` to `to` (trimmed, and checked as
/// [`check_name`] does), keeping its place. The error is the text to show.
// Used first by the sessions menu (task 5).
#[allow(dead_code)]
pub(crate) fn rename(path: &Path, from: &str, to: &str) -> Result<(), String> {
    let to = check_name(to).map_err(str::to_owned)?;
    let mut sessions =
        read(path).map_err(|why| format!("Could not read the saved sessions: {why}"))?;
    let index = sessions
        .iter()
        .position(|saved| saved.name == from)
        .ok_or("That session is no longer saved.")?;
    if to == from {
        return Ok(());
    }
    if sessions.iter().any(|saved| saved.name == to) {
        return Err("A session with this name exists.".to_owned());
    }
    sessions[index].name = to;
    write(path, &sessions).map_err(|err| format!("Could not save the sessions: {err}"))
}

/// Remove the session `name`; nothing to do if there is none (another window
/// may have removed it). A file that cannot be read is an error and is left as
/// it is.
// Used first by the sessions menu (task 5).
#[allow(dead_code)]
pub(crate) fn delete(path: &Path, name: &str) -> io::Result<()> {
    let mut sessions = read(path).map_err(|why| unreadable_error(&why))?;
    let before = sessions.len();
    sessions.retain(|saved| saved.name != name);
    if sessions.len() == before {
        return Ok(());
    }
    write(path, &sessions)
}

/// "Session N" for the smallest N from 1 that no session in `sessions` uses.
// Used first by the sessions menu (task 5).
#[allow(dead_code)]
pub(crate) fn next_default_name(sessions: &[Session]) -> String {
    let mut n = 1_u64;
    loop {
        let name = format!("Session {n}");
        if !sessions.iter().any(|saved| saved.name == name) {
            return name;
        }
        n += 1;
    }
}

/// The error for a file that cannot be used, as the writes report it.
fn unreadable_error(why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("Could not read the saved sessions: {why}"),
    )
}

// Reading

/// The sessions in the file at `path` (none if there is no file), or why the
/// file cannot be used.
fn read(path: &Path) -> Result<Vec<Session>, String> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(format!("it could not be read ({err})")),
    };
    // serde_json refuses nesting past 128 levels before it builds anything
    // deeper, so what follows recurses a bounded depth.
    let value: Value = serde_json::from_slice(&bytes).map_err(|err| {
        let text = err.to_string();
        if text.starts_with("recursion limit exceeded") {
            "it is nested too deeply".to_owned()
        } else {
            // Syntax errors say where, never what the file said.
            format!("it is not JSON ({text})")
        }
    })?;
    let fields = value.as_object().ok_or(NOT_A_LIST)?;
    match fields.get("version") {
        Some(Value::Number(n)) if n.as_u64() == Some(VERSION) => {}
        Some(Value::Number(n)) => {
            return Err(format!(
                "it is version {n}; this Moniker reads version {VERSION}"
            ));
        }
        _ => {
            return Err(format!(
                "it has no version number; this Moniker reads version {VERSION}"
            ));
        }
    }
    let entries = fields
        .get("sessions")
        .and_then(Value::as_array)
        .ok_or(NOT_A_LIST)?;
    let mut sessions = Vec::new();
    let mut names = HashSet::new();
    for entry in entries {
        // A name already seen is skipped: the first one stays.
        if let Some(session) = session_from(entry)
            && names.insert(session.name.clone())
        {
            sessions.push(session);
        }
    }
    Ok(sessions)
}

const NOT_A_LIST: &str = "it is not a list of sessions";

/// The session in `value`, or `None` if it makes no sense: a name that fails
/// [`check_name`], no tab, or a tab [`tab_from`] refuses. (A name that is
/// already taken is for the caller to see.)
fn session_from(value: &Value) -> Option<Session> {
    let fields = value.as_object()?;
    let name = check_name(fields.get("name")?.as_str()?).ok()?;
    let tabs = fields
        .get("tabs")?
        .as_array()?
        .iter()
        .map(tab_from)
        .collect::<Option<Vec<_>>>()?;
    if tabs.is_empty() {
        return None;
    }
    Some(Session { name, tabs })
}

/// A tab: its split tree and the pane in focus. `focused` that is missing, is
/// not a whole number or is past the last pane is the first pane.
fn tab_from(value: &Value) -> Option<TabRecord> {
    let fields = value.as_object()?;
    let mut panes = 0;
    let root = node_from(fields.get("root")?, 0, &mut panes)?;
    let focused = fields
        .get("focused")
        .and_then(Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
        .filter(|&index| index < panes)
        .unwrap_or(0);
    Some(TabRecord { root, focused })
}

/// A node of a split tree: `depth` splits are above it, `panes` counts the
/// panes read so far in the tab. `None` as soon as the tree is nested past
/// [`MAX_SPLIT_DEPTH`], holds more than [`MAX_PANES`] panes, or has a node that
/// is not one of the forms.
fn node_from(value: &Value, depth: usize, panes: &mut usize) -> Option<Node> {
    let fields = value.as_object()?;
    if let Some(direction) = fields.get("split") {
        if depth >= MAX_SPLIT_DEPTH {
            return None;
        }
        let horizontal = match direction.as_str()? {
            "horizontal" => true,
            "vertical" => false,
            _ => return None,
        };
        let fraction = fields
            .get("fraction")
            .and_then(Value::as_f64)
            .filter(|fraction| fraction.is_finite())
            .map_or(FRACTION_DEFAULT, |fraction| {
                fraction.clamp(FRACTION_RANGE.0, FRACTION_RANGE.1)
            });
        let start = node_from(fields.get("start")?, depth + 1, panes)?;
        let end = node_from(fields.get("end")?, depth + 1, panes)?;
        return Some(Node::Split {
            horizontal,
            fraction,
            start: Box::new(start),
            end: Box::new(end),
        });
    }
    *panes += 1;
    if *panes > MAX_PANES {
        return None;
    }
    pane_from(fields).map(Node::Pane)
}

/// A pane. The kind and the texts that say which profile it was must make
/// sense; the name, text size and transcript fall back to none, the normal
/// size and no when they are missing or of the wrong type.
fn pane_from(fields: &Map<String, Value>) -> Option<PaneRecord> {
    let kind = match fields.get("pane")?.as_str()? {
        "profile" => {
            let id = fields.get("profile")?.as_str()?;
            if id.chars().count() > MAX_ID_CHARS {
                return None;
            }
            Kind::Profile {
                id: id.to_owned(),
                name: label(fields.get("profile_name")?.as_str()?)?,
            }
        }
        "local" => Kind::Local {
            dir: fields
                .get("dir")
                .and_then(Value::as_str)
                // A path that cannot be one (a NUL cannot be in a path) is no
                // folder, as a relative one is not.
                .filter(|dir| Path::new(dir).is_absolute() && !dir.contains('\0'))
                .map(PathBuf::from),
        },
        "list" => Kind::List,
        _ => return None,
    };
    let name = match fields.get("name") {
        Some(Value::String(name)) => Some(label(name)?),
        _ => None,
    };
    Some(PaneRecord {
        kind,
        name,
        scale: fields
            .get("scale")
            .and_then(Value::as_f64)
            .map_or(zoom::DEFAULT, zoom::clamp),
        transcript: fields
            .get("transcript")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// A pane's or a profile's name as it is kept: as given, if it has no control
/// character and is at most [`MAX_LABEL_CHARS`] characters.
fn label(text: &str) -> Option<String> {
    if text.chars().any(char::is_control) || text.chars().count() > MAX_LABEL_CHARS {
        return None;
    }
    Some(text.to_owned())
}

// Writing

/// Write `sessions` as the file at `path`: pretty JSON with a trailing
/// newline, through [`write_private`].
fn write(path: &Path, sessions: &[Session]) -> io::Result<()> {
    let entries = sessions
        .iter()
        .map(session_value)
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "a session is too deep"))?;
    let file = json!({ "version": VERSION, "sessions": entries });
    let mut bytes = serde_json::to_vec_pretty(&file).map_err(io::Error::other)?;
    bytes.push(b'\n');
    write_private(path, &bytes)
}

/// Whether reading would find `session` again under the same name (its
/// fractions and text sizes it would bring into range, the rest as it is).
fn is_storable(session: &Session) -> bool {
    session_value(session)
        .and_then(|value| session_from(&value))
        .is_some_and(|read| read.name == session.name)
}

/// `session` as JSON; `None` if it nests past [`MAX_SPLIT_DEPTH`] (so building
/// the value takes a bounded depth of stack, whatever the tree).
fn session_value(session: &Session) -> Option<Value> {
    let tabs = session
        .tabs
        .iter()
        .map(|tab| {
            Some(json!({
                "focused": tab.focused,
                "root": node_value(&tab.root, 0)?,
            }))
        })
        .collect::<Option<Vec<_>>>()?;
    Some(json!({ "name": session.name, "tabs": tabs }))
}

fn node_value(node: &Node, depth: usize) -> Option<Value> {
    match node {
        Node::Pane(pane) => Some(pane_value(pane)),
        Node::Split {
            horizontal,
            fraction,
            start,
            end,
        } => {
            if depth >= MAX_SPLIT_DEPTH {
                return None;
            }
            Some(json!({
                "split": if *horizontal { "horizontal" } else { "vertical" },
                "fraction": fraction,
                "start": node_value(start, depth + 1)?,
                "end": node_value(end, depth + 1)?,
            }))
        }
    }
}

fn pane_value(pane: &PaneRecord) -> Value {
    let mut fields = Map::new();
    match &pane.kind {
        Kind::Profile { id, name } => {
            fields.insert("pane".to_owned(), json!("profile"));
            fields.insert("profile".to_owned(), json!(id));
            fields.insert("profile_name".to_owned(), json!(name));
        }
        Kind::Local { dir } => {
            fields.insert("pane".to_owned(), json!("local"));
            // A path that is not text cannot be in a JSON file: it is saved as
            // no folder, and the shell starts at home.
            let dir = dir.as_deref().and_then(Path::to_str);
            fields.insert("dir".to_owned(), json!(dir));
        }
        Kind::List => {
            fields.insert("pane".to_owned(), json!("list"));
        }
    }
    fields.insert("name".to_owned(), json!(pane.name));
    fields.insert("scale".to_owned(), json!(pane.scale));
    fields.insert("transcript".to_owned(), json!(pane.transcript));
    Value::Object(fields)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::PermissionsExt;

    const ID: &str = "0b7c6f4e-2f58-4c1b-9a51-3a1d9f3c2e10";
    const LIST: &str = r#"{"pane":"list"}"#;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn pane(kind: Kind) -> PaneRecord {
        PaneRecord {
            kind,
            name: None,
            scale: 1.0,
            transcript: false,
        }
    }

    fn leaf(kind: Kind) -> Node {
        Node::Pane(pane(kind))
    }

    /// A session of one tab with one new-tab list pane.
    fn session(name: &str) -> Session {
        Session {
            name: name.to_owned(),
            tabs: vec![TabRecord {
                root: leaf(Kind::List),
                focused: 0,
            }],
        }
    }

    /// The text of a session of one tab holding `root`.
    fn session_text(name: &str, root: &str) -> String {
        let name = serde_json::to_string(name).unwrap();
        format!(r#"{{"name":{name},"tabs":[{{"focused":0,"root":{root}}}]}}"#)
    }

    /// The text of a file holding the sessions in `entries`.
    fn file_text(entries: &[String]) -> String {
        format!(r#"{{"version":1,"sessions":[{}]}}"#, entries.join(","))
    }

    /// A tree of exactly `leaves` panes, as balanced as it can be.
    fn tree_text(leaves: usize) -> String {
        if leaves == 1 {
            return LIST.to_owned();
        }
        let left = leaves / 2;
        format!(
            r#"{{"split":"vertical","fraction":0.5,"start":{},"end":{}}}"#,
            tree_text(left),
            tree_text(leaves - left)
        )
    }

    /// A tree of `splits` splits one inside the other, two panes at the bottom
    /// and one beside each split.
    fn chain_text(splits: usize) -> String {
        let mut text = LIST.to_owned();
        for _ in 0..splits {
            text = format!(r#"{{"split":"horizontal","start":{text},"end":{LIST}}}"#);
        }
        text
    }

    fn count_panes(node: &Node) -> usize {
        match node {
            Node::Pane(_) => 1,
            Node::Split { start, end, .. } => count_panes(start) + count_panes(end),
        }
    }

    fn write_text(path: &Path, text: &str) {
        fs::write(path, text).unwrap();
    }

    fn sessions_at(path: &Path) -> Vec<Session> {
        match load(path) {
            Loaded::Sessions(sessions) => sessions,
            Loaded::Unreadable(why) => panic!("unreadable: {why}"),
        }
    }

    fn names_at(path: &Path) -> Vec<String> {
        sessions_at(path).into_iter().map(|s| s.name).collect()
    }

    /// The only session a file holds, when it holds one.
    fn only_session(text: &str) -> Option<Session> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        write_text(&path, text);
        let mut sessions = sessions_at(&path);
        assert!(sessions.len() <= 1, "{text}");
        sessions.pop()
    }

    /// The pane of a one-pane session read from `root`.
    fn pane_of(root: &str) -> Option<PaneRecord> {
        let session = only_session(&file_text(&[session_text("S", root)]))?;
        match session.tabs.into_iter().next()?.root {
            Node::Pane(pane) => Some(pane),
            Node::Split { .. } => panic!("a split"),
        }
    }

    #[test]
    fn a_session_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        assert_eq!(load(&path), Loaded::Sessions(vec![]), "no file");

        let work = Session {
            name: "<b>Work</b> &amp; play".to_owned(),
            tabs: vec![
                TabRecord {
                    root: Node::Split {
                        horizontal: true,
                        fraction: 0.3,
                        start: Box::new(Node::Pane(PaneRecord {
                            kind: Kind::Profile {
                                id: ID.to_owned(),
                                name: "<i>web</i>".to_owned(),
                            },
                            name: Some("Front <end>".to_owned()),
                            scale: 1.5,
                            transcript: true,
                        })),
                        end: Box::new(Node::Split {
                            horizontal: false,
                            fraction: 0.7,
                            start: Box::new(Node::Pane(PaneRecord {
                                kind: Kind::Local {
                                    dir: Some(PathBuf::from("/home/u/My Files/Ünï")),
                                },
                                name: None,
                                scale: 0.8,
                                transcript: false,
                            })),
                            end: Box::new(leaf(Kind::List)),
                        }),
                    },
                    focused: 2,
                },
                TabRecord {
                    root: Node::Pane(PaneRecord {
                        kind: Kind::Local { dir: None },
                        name: Some("Notes".to_owned()),
                        scale: 1.0,
                        transcript: false,
                    }),
                    focused: 0,
                },
            ],
        };
        let other = session("Other");
        put(&path, work.clone(), false).unwrap();
        put(&path, other.clone(), false).unwrap();
        assert_eq!(load(&path), Loaded::Sessions(vec![work, other]));

        // Pretty-printed like the settings, with a trailing newline.
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.ends_with("}\n"), "{text:?}");
        assert!(text.contains("\n  \"sessions\""), "{text}");
        assert!(text.contains("\"version\": 1"), "{text}");
    }

    #[test]
    fn a_folder_that_is_not_text_is_not_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let odd = PathBuf::from(OsString::from_vec(b"/home/u/\xff".to_vec()));
        let mut saved = session("S");
        saved.tabs[0].root = leaf(Kind::Local { dir: Some(odd) });
        put(&path, saved, false).unwrap();
        let loaded = sessions_at(&path);
        let Node::Pane(pane) = &loaded[0].tabs[0].root else {
            panic!("a split")
        };
        assert_eq!(pane.kind, Kind::Local { dir: None });
    }

    #[test]
    fn a_bad_entry_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        write_text(
            &path,
            &file_text(&[
                session_text("Bad", r#"{"pane":"telnet"}"#),
                session_text("Good", LIST),
            ]),
        );
        assert_eq!(names_at(&path), ["Good"]);

        // An entry that is not even an object, and every way a session or a
        // node can be wrong, each on its own beside a good session.
        let id65 = "x".repeat(65);
        let id64 = "x".repeat(64);
        let long_label = "x".repeat(201);
        let bad = [
            "5".to_owned(),
            r#""Work""#.to_owned(),
            "null".to_owned(),
            r#"{"tabs":[{"root":{"pane":"list"}}]}"#.to_owned(),
            r#"{"name":7,"tabs":[{"root":{"pane":"list"}}]}"#.to_owned(),
            r#"{"name":"S"}"#.to_owned(),
            r#"{"name":"S","tabs":[]}"#.to_owned(),
            r#"{"name":"S","tabs":{"root":{"pane":"list"}}}"#.to_owned(),
            r#"{"name":"S","tabs":[{"focused":0}]}"#.to_owned(),
            r#"{"name":"S","tabs":[5]}"#.to_owned(),
            // One bad tab costs the whole session.
            r#"{"name":"S","tabs":[{"root":{"pane":"list"}},{"root":{"pane":"x"}}]}"#.to_owned(),
            session_text("", LIST),
            session_text("   ", LIST),
            session_text("a\tb", LIST),
            session_text(&"n".repeat(101), LIST),
            session_text("S", "5"),
            session_text("S", "[]"),
            session_text("S", "{}"),
            session_text("S", r#"{"pane":5}"#),
            session_text("S", r#"{"pane":"LIST"}"#),
            session_text("S", r#"{"pane":"profile","profile_name":"web"}"#),
            session_text("S", r#"{"pane":"profile","profile":"ok"}"#),
            session_text(
                "S",
                r#"{"pane":"profile","profile":7,"profile_name":"web"}"#,
            ),
            session_text("S", r#"{"pane":"profile","profile":"p","profile_name":7}"#),
            session_text(
                "S",
                &format!(r#"{{"pane":"profile","profile":"{id65}","profile_name":"web"}}"#),
            ),
            session_text(
                "S",
                &format!(r#"{{"pane":"profile","profile":"p","profile_name":"{long_label}"}}"#),
            ),
            session_text(
                "S",
                r#"{"pane":"profile","profile":"p","profile_name":"we\nb"}"#,
            ),
            session_text("S", &format!(r#"{{"pane":"list","name":"{long_label}"}}"#)),
            session_text("S", r#"{"pane":"list","name":"a\u0007b"}"#),
            session_text(
                "S",
                r#"{"split":"diagonal","start":{"pane":"list"},"end":{"pane":"list"}}"#,
            ),
            session_text(
                "S",
                r#"{"split":7,"start":{"pane":"list"},"end":{"pane":"list"}}"#,
            ),
            session_text("S", r#"{"split":"vertical","end":{"pane":"list"}}"#),
            session_text("S", r#"{"split":"vertical","start":{"pane":"list"}}"#),
            session_text(
                "S",
                r#"{"split":"vertical","start":{"pane":"list"},"end":{"pane":"x"}}"#,
            ),
        ];
        for entry in &bad {
            write_text(
                &path,
                &file_text(&[entry.clone(), session_text("Good", LIST)]),
            );
            assert_eq!(names_at(&path), ["Good"], "{entry}");
        }

        // The edges of what is allowed are not bad.
        let fine = [
            session_text(
                "S",
                &format!(
                    r#"{{"pane":"profile","profile":"{id64}","profile_name":"{}"}}"#,
                    "x".repeat(200)
                ),
            ),
            session_text(
                "S",
                &format!(r#"{{"pane":"list","name":"{}"}}"#, "x".repeat(200)),
            ),
            session_text("S", r#"{"pane":"list","name":null}"#),
            session_text("S", r#"{"pane":"list","name":7}"#),
            session_text("S", r#"{"pane":"profile","profile":"","profile_name":""}"#),
        ];
        for entry in &fine {
            write_text(&path, &file_text(std::slice::from_ref(entry)));
            assert_eq!(names_at(&path), ["S"], "{entry}");
        }

        // A name used twice: the first stays, the second is skipped. A name
        // is read trimmed, as a saved one would have been.
        write_text(
            &path,
            &file_text(&[
                session_text(" Work ", LIST),
                session_text("Other", LIST),
                session_text("Work", &chain_text(1)),
            ]),
        );
        let loaded = sessions_at(&path);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].name, "Work");
        assert_eq!(loaded[0].tabs[0].root, leaf(Kind::List), "the first one");
        assert_eq!(loaded[1].name, "Other");
    }

    #[test]
    fn a_pane_is_read_field_by_field() {
        let pane = pane_of(&format!(
            r#"{{"pane":"profile","profile":"{ID}","profile_name":"web","name":"Front","scale":1.2,"transcript":true}}"#
        ))
        .unwrap();
        assert_eq!(
            pane,
            PaneRecord {
                kind: Kind::Profile {
                    id: ID.to_owned(),
                    name: "web".to_owned()
                },
                name: Some("Front".to_owned()),
                scale: 1.2,
                transcript: true,
            }
        );

        // Everything but the kind may be missing or of the wrong type.
        for extras in [
            "",
            r#","name":null,"scale":null,"transcript":null"#,
            r#","name":5,"scale":"big","transcript":"yes""#,
        ] {
            let pane = pane_of(&format!(r#"{{"pane":"list"{extras}}}"#)).unwrap();
            assert_eq!(pane, self::pane(Kind::List), "{extras}");
        }

        // A folder is an absolute path or nothing.
        for (dir, want) in [
            (r#""/home/u""#, Some("/home/u")),
            (r#""/home/u/My Files""#, Some("/home/u/My Files")),
            (r#""relative/dir""#, None),
            (r#""~/dir""#, None),
            (r#""""#, None),
            (r#""/a\u0000b""#, None),
            ("7", None),
            ("null", None),
        ] {
            let pane = pane_of(&format!(r#"{{"pane":"local","dir":{dir}}}"#)).unwrap();
            let want = want.map(PathBuf::from);
            assert_eq!(pane.kind, Kind::Local { dir: want }, "{dir}");
        }
        let pane = pane_of(r#"{"pane":"local"}"#).unwrap();
        assert_eq!(pane.kind, Kind::Local { dir: None });
    }

    #[test]
    fn the_focused_pane_is_one_of_the_panes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let three = r#"{"split":"vertical","start":{"pane":"list"},"end":{"split":"horizontal","start":{"pane":"list"},"end":{"pane":"list"}}}"#;
        for (focused, want) in [
            ("0", 0),
            ("1", 1),
            ("2", 2),
            ("3", 0),
            ("1000000", 0),
            ("-1", 0),
            ("1.5", 0),
            (r#""2""#, 0),
            ("null", 0),
        ] {
            let text = format!(
                r#"{{"version":1,"sessions":[{{"name":"S","tabs":[{{"focused":{focused},"root":{three}}}]}}]}}"#
            );
            write_text(&path, &text);
            assert_eq!(sessions_at(&path)[0].tabs[0].focused, want, "{focused}");
        }
        let text =
            format!(r#"{{"version":1,"sessions":[{{"name":"S","tabs":[{{"root":{three}}}]}}]}}"#);
        write_text(&path, &text);
        assert_eq!(sessions_at(&path)[0].tabs[0].focused, 0, "missing");
    }

    #[test]
    fn a_crafted_file_stays_harmless() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let good = session_text("Good", LIST);

        // 16 splits one inside the other are fine, 17 are not.
        write_text(&path, &file_text(&[session_text("Deep", &chain_text(16))]));
        assert_eq!(names_at(&path), ["Deep"], "16 deep");
        write_text(
            &path,
            &file_text(&[session_text("Deep", &chain_text(17)), good.clone()]),
        );
        assert_eq!(names_at(&path), ["Good"], "17 deep");
        write_text(
            &path,
            &file_text(&[session_text("Deep", &chain_text(100)), good.clone()]),
        );
        assert_eq!(names_at(&path), ["Good"], "100 deep");

        // 32 panes in a tab are fine, 33 are not (and neither are hundreds).
        write_text(&path, &file_text(&[session_text("Wide", &tree_text(32))]));
        let loaded = sessions_at(&path);
        assert_eq!(count_panes(&loaded[0].tabs[0].root), 32);
        write_text(
            &path,
            &file_text(&[session_text("Wide", &tree_text(33)), good.clone()]),
        );
        assert_eq!(names_at(&path), ["Good"], "33 panes");
        write_text(
            &path,
            &file_text(&[session_text("Wide", &tree_text(500)), good.clone()]),
        );
        assert_eq!(names_at(&path), ["Good"], "500 panes");

        // The limit is per tab: two tabs of 32 panes are fine.
        let two = format!(
            r#"{{"name":"Two","tabs":[{{"root":{t}}},{{"root":{t}}}]}}"#,
            t = tree_text(32)
        );
        write_text(&path, &file_text(&[two]));
        assert_eq!(sessions_at(&path)[0].tabs.len(), 2);

        // Nesting past serde_json's own limit is a file that cannot be read,
        // and reading it takes no stack (this runs on a test thread's).
        let n = 10_000;
        let text = format!(
            r#"{{"version":1,"sessions":[{{"name":"x","tabs":[{{"root":{}{LIST}{}}}]}}]}}"#,
            r#"{"split":"vertical","start":"#.repeat(n),
            r#","end":{"pane":"list"}}"#.repeat(n)
        );
        write_text(&path, &text);
        match load(&path) {
            Loaded::Unreadable(why) => assert!(why.contains("nested"), "{why}"),
            other => panic!("{other:?}"),
        }
        let text = format!("{}{}", "[".repeat(n), "]".repeat(n));
        write_text(&path, &text);
        assert!(matches!(load(&path), Loaded::Unreadable(_)));

        // A number too big to be a number is a file that is not JSON.
        write_text(
            &path,
            r#"{"version":1,"sessions":[{"name":"x","tabs":[{"root":{"pane":"list","scale":1e999}}]}]}"#,
        );
        assert!(matches!(load(&path), Loaded::Unreadable(_)));

        // Not a thing in the file is kept as a thing: a name with markup or a
        // path with odd characters is only text.
        write_text(
            &path,
            &file_text(&[session_text(
                "<b>x</b>",
                r#"{"pane":"local","dir":"/tmp/\n$(x)"}"#,
            )]),
        );
        assert_eq!(names_at(&path), ["<b>x</b>"]);
    }

    #[test]
    fn a_split_names_its_direction() {
        // The words in the file, written and read.
        let split = |direction: &str| {
            let root = format!(
                r#"{{"split":"{direction}","start":{{"pane":"list"}},"end":{{"pane":"list"}}}}"#
            );
            only_session(&file_text(&[session_text("S", &root)]))
                .unwrap()
                .tabs[0]
                .root
                .clone()
        };
        let horizontal = |node: Node| match node {
            Node::Split { horizontal, .. } => horizontal,
            Node::Pane(_) => panic!("a pane"),
        };
        assert!(horizontal(split("horizontal")));
        assert!(!horizontal(split("vertical")));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        for (side_by_side, word) in [(true, "horizontal"), (false, "vertical")] {
            let mut saved = session("S");
            saved.tabs[0].root = Node::Split {
                horizontal: side_by_side,
                fraction: 0.5,
                start: Box::new(leaf(Kind::List)),
                end: Box::new(leaf(Kind::List)),
            };
            put(&path, saved, false).unwrap();
            let text = fs::read_to_string(&path).unwrap();
            assert!(text.contains(&format!("\"split\": \"{word}\"")), "{text}");
        }
    }

    #[test]
    fn fractions_and_scales_are_clamped() {
        let split = |fraction: &str| {
            let root = format!(
                r#"{{"split":"vertical"{fraction},"start":{{"pane":"list"}},"end":{{"pane":"list"}}}}"#
            );
            let session = only_session(&file_text(&[session_text("S", &root)])).unwrap();
            match session.tabs[0].root {
                Node::Split { fraction, .. } => fraction,
                Node::Pane(_) => panic!("a pane"),
            }
        };
        assert_eq!(split(r#","fraction":0.3"#), 0.3);
        assert_eq!(split(r#","fraction":0.05"#), 0.05);
        assert_eq!(split(r#","fraction":0.95"#), 0.95);
        assert_eq!(split(r#","fraction":2.0"#), 0.95);
        assert_eq!(split(r#","fraction":1"#), 0.95);
        assert_eq!(split(r#","fraction":0.04"#), 0.05);
        assert_eq!(split(r#","fraction":0"#), 0.05);
        assert_eq!(split(r#","fraction":-3"#), 0.05);
        assert_eq!(split(r#","fraction":1e300"#), 0.95);
        assert_eq!(split(r#","fraction":"x""#), 0.5);
        assert_eq!(split(r#","fraction":null"#), 0.5);
        assert_eq!(split(r#","fraction":[]"#), 0.5);
        assert_eq!(split(""), 0.5);

        let scale = |scale: &str| {
            pane_of(&format!(r#"{{"pane":"list","scale":{scale}}}"#))
                .unwrap()
                .scale
        };
        assert_eq!(scale("1.5"), 1.5);
        assert_eq!(scale("1e9"), zoom::MAX);
        assert_eq!(scale("3.0"), zoom::MAX);
        assert_eq!(scale("0"), zoom::MIN);
        assert_eq!(scale("-4"), zoom::MIN);
        assert_eq!(scale("0.1"), zoom::MIN);
        assert_eq!(scale(r#""big""#), zoom::DEFAULT);
        assert_eq!(scale("null"), zoom::DEFAULT);
        assert_eq!(
            pane_of(r#"{"pane":"list"}"#).unwrap().scale,
            zoom::DEFAULT,
            "missing"
        );
    }

    #[test]
    fn a_bad_file_is_told_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let bad: [&[u8]; 11] = [
            b"hunter2 is not json",
            b"",
            b"{\"version\":1,\"sessions\":[",
            b"{\"version\":2,\"sessions\":[]}",
            b"{\"version\":0,\"sessions\":[]}",
            b"{\"version\":\"1\",\"sessions\":[]}",
            b"{\"sessions\":[]}",
            b"{\"version\":1}",
            b"{\"version\":1,\"sessions\":{\"hunter2\":1}}",
            b"[\"hunter2\"]",
            b"\xff\xfe\"hunter2\"",
        ];
        for bytes in bad {
            fs::write(&path, bytes).unwrap();
            let Loaded::Unreadable(why) = load(&path) else {
                panic!("{:?} was read", String::from_utf8_lossy(bytes))
            };
            assert!(!why.is_empty() && !why.contains('\n'), "{why:?}");
            assert!(!why.contains("hunter2"), "the file's text is shown: {why}");

            // Nothing writes over it, whatever the change, and each says why.
            let told = "Could not read the saved sessions: it ";
            let error = put(&path, session("New"), false).unwrap_err();
            assert!(error.to_string().starts_with(told), "{error}");
            let error = rename(&path, "a", "b").unwrap_err();
            assert!(error.starts_with(told), "{error}");
            let error = delete(&path, "a").unwrap_err();
            assert!(error.to_string().starts_with(told), "{error}");
            assert_eq!(fs::read(&path).unwrap(), bytes);

            // Unless asked to, and then it holds only the new session.
            put(&path, session("New"), true).unwrap();
            assert_eq!(sessions_at(&path), [session("New")]);
        }

        // The words the menu shows.
        fs::write(&path, r#"{"version":2,"sessions":[]}"#).unwrap();
        assert_eq!(
            load(&path),
            Loaded::Unreadable("it is version 2; this Moniker reads version 1".to_owned())
        );
        fs::write(&path, r#"{"version":1,"sessions":5}"#).unwrap();
        assert_eq!(
            load(&path),
            Loaded::Unreadable("it is not a list of sessions".to_owned())
        );
        fs::write(&path, "not json").unwrap();
        let Loaded::Unreadable(why) = load(&path) else {
            panic!("read")
        };
        assert!(why.starts_with("it is not JSON"), "{why}");

        // A path that is a folder cannot be read, and is told, not replaced.
        let folder = dir.path().join("folder");
        fs::create_dir(&folder).unwrap();
        assert!(matches!(load(&folder), Loaded::Unreadable(_)));
        assert!(put(&folder, session("New"), false).is_err());
    }

    #[test]
    fn names_are_checked() {
        assert_eq!(check_name(""), Err("Enter a name."));
        assert_eq!(check_name("  "), Err("Enter a name."));
        assert_eq!(check_name("\u{3000}\n"), Err("Enter a name."));
        const CONTROL: &str = "A name cannot contain control characters.";
        assert_eq!(check_name("a\tb"), Err(CONTROL));
        assert_eq!(check_name("a\nb"), Err(CONTROL));
        assert_eq!(check_name("a\u{7f}b"), Err(CONTROL));
        assert_eq!(check_name("a\u{85}b"), Err(CONTROL));
        assert_eq!(check_name("\u{1b}"), Err(CONTROL));
        const LONG: &str = "A name can be at most 100 characters.";
        assert_eq!(check_name(&"n".repeat(101)), Err(LONG));
        assert_eq!(check_name(&"n".repeat(1000)), Err(LONG));
        assert_eq!(check_name(&"é".repeat(101)), Err(LONG));
        // Empty comes before control, control before long.
        assert_eq!(check_name(&format!("a\t{}", "n".repeat(200))), Err(CONTROL));

        assert_eq!(check_name(" Work "), Ok("Work".to_owned()));
        assert_eq!(check_name("\tWork\n"), Ok("Work".to_owned()));
        assert_eq!(check_name("Two words"), Ok("Two words".to_owned()));
        assert_eq!(check_name(&"n".repeat(100)), Ok("n".repeat(100)));
        // Characters, not bytes.
        assert_eq!(check_name(&"é".repeat(100)), Ok("é".repeat(100)));
        // The length is of the trimmed name.
        let padded = format!("  {}  ", "n".repeat(100));
        assert_eq!(check_name(&padded), Ok("n".repeat(100)));
        assert_eq!(
            check_name("<b>x</b> &amp;"),
            Ok("<b>x</b> &amp;".to_owned())
        );
    }

    #[test]
    fn saving_keeps_a_session_saved_meanwhile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        // This window loaded the file while it was empty...
        assert_eq!(load(&path), Loaded::Sessions(vec![]));
        // ...another window saved B...
        put(&path, session("B"), false).unwrap();
        // ...and now this one saves A.
        put(&path, session("A"), false).unwrap();
        assert_eq!(names_at(&path), ["B", "A"]);
    }

    #[test]
    fn saving_under_a_used_name_replaces_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        put(&path, session("A"), false).unwrap();
        put(&path, session("B"), false).unwrap();
        put(&path, session("C"), false).unwrap();
        let mut again = session("B");
        again.tabs[0].focused = 0;
        again.tabs[0].root = leaf(Kind::Local {
            dir: Some(PathBuf::from("/tmp")),
        });
        put(&path, again.clone(), false).unwrap();
        // In its place, and only one of the name.
        assert_eq!(names_at(&path), ["A", "B", "C"]);
        assert_eq!(sessions_at(&path)[1], again);
    }

    #[test]
    fn a_session_that_would_not_read_back_is_not_saved() {
        // `splits` splits one inside the other: `splits + 1` panes.
        fn chain(splits: usize) -> Node {
            let mut node = leaf(Kind::List);
            for _ in 0..splits {
                node = Node::Split {
                    horizontal: true,
                    fraction: 0.5,
                    start: Box::new(node),
                    end: Box::new(leaf(Kind::List)),
                };
            }
            node
        }
        // Exactly `leaves` panes in the fewest splits deep.
        fn balanced(leaves: usize) -> Node {
            if leaves == 1 {
                return leaf(Kind::List);
            }
            Node::Split {
                horizontal: false,
                fraction: 0.5,
                start: Box::new(balanced(leaves / 2)),
                end: Box::new(balanced(leaves - leaves / 2)),
            }
        }
        fn with_root(root: Node) -> Session {
            Session {
                name: "S".to_owned(),
                tabs: vec![TabRecord { root, focused: 0 }],
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        put(&path, session("Kept"), false).unwrap();
        let before = fs::read(&path).unwrap();

        let bad = [
            session(""),
            session("a\tb"),
            session(" padded "),
            session(&"n".repeat(101)),
            Session {
                name: "S".to_owned(),
                tabs: vec![],
            },
            with_root(chain(17)),
            with_root(chain(1000)),
            with_root(balanced(33)),
            with_root(leaf(Kind::Profile {
                id: "x".repeat(65),
                name: "web".to_owned(),
            })),
            with_root(leaf(Kind::Profile {
                id: "x".to_owned(),
                name: "x".repeat(201),
            })),
            with_root(Node::Pane(PaneRecord {
                name: Some("a\nb".to_owned()),
                ..pane(Kind::List)
            })),
        ];
        for session in bad {
            assert!(put(&path, session.clone(), false).is_err(), "{session:?}");
            assert!(put(&path, session, true).is_err());
            assert_eq!(fs::read(&path).unwrap(), before, "the file is as it was");
        }

        // The edges of what is allowed are saved.
        put(&path, with_root(chain(16)), false).unwrap();
        put(&path, with_root(balanced(32)), false).unwrap();
        assert_eq!(names_at(&path), ["Kept", "S"]);

        // A refusal creates no file.
        let fresh = dir.path().join("fresh.json");
        assert!(put(&fresh, session(""), false).is_err());
        assert!(!fresh.exists());
    }

    #[test]
    fn rename_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let mut a = session("A");
        a.tabs[0].root = leaf(Kind::Local { dir: None });
        put(&path, a.clone(), false).unwrap();
        put(&path, session("B"), false).unwrap();
        put(&path, session("C"), false).unwrap();

        assert_eq!(
            rename(&path, "A", "B"),
            Err("A session with this name exists.".to_owned())
        );
        assert_eq!(rename(&path, "A", "  "), Err("Enter a name.".to_owned()));
        assert_eq!(
            rename(&path, "A", "a\tb"),
            Err("A name cannot contain control characters.".to_owned())
        );
        assert_eq!(
            rename(&path, "A", &"n".repeat(101)),
            Err("A name can be at most 100 characters.".to_owned())
        );
        let Err(why) = rename(&path, "Nope", "Z") else {
            panic!("renamed a session that is not there")
        };
        assert!(why.contains("no longer saved"), "{why}");
        assert_eq!(names_at(&path), ["A", "B", "C"], "nothing changed");

        // Renamed in place, trimmed, with what it holds.
        assert_eq!(rename(&path, "A", " Z "), Ok(()));
        let loaded = sessions_at(&path);
        assert_eq!(
            loaded.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["Z", "B", "C"]
        );
        assert_eq!(loaded[0].tabs, a.tabs);
        // To its own name is nothing to do.
        assert_eq!(rename(&path, "Z", "Z"), Ok(()));
        assert_eq!(rename(&path, "Z", " Z "), Ok(()));
        assert_eq!(names_at(&path), ["Z", "B", "C"]);
        // The old name is free again.
        assert_eq!(rename(&path, "B", "A"), Ok(()));
        assert_eq!(names_at(&path), ["Z", "A", "C"]);

        delete(&path, "A").unwrap();
        assert_eq!(names_at(&path), ["Z", "C"]);
        // Already gone (another window deleted it): nothing to do.
        delete(&path, "A").unwrap();
        assert_eq!(names_at(&path), ["Z", "C"]);
        delete(&path, "Z").unwrap();
        delete(&path, "C").unwrap();
        assert_eq!(names_at(&path), Vec::<String>::new());
        assert!(path.exists(), "an empty list is still a file");

        // With no file there is nothing to delete or rename, and none is made.
        let none = dir.path().join("none.json");
        delete(&none, "A").unwrap();
        assert!(rename(&none, "A", "B").is_err());
        assert!(!none.exists());
    }

    #[test]
    fn the_file_is_private() {
        let dir = tempfile::tempdir().unwrap();
        // The directory may not exist yet.
        let folder = dir.path().join("cfg").join("moniker");
        let path = folder.join(FILE_NAME);
        put(&path, session("A"), false).unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&folder), 0o700);

        // Saving again, and renaming and deleting, replace the file and keep
        // it private, with no temporary file left beside it.
        put(&path, session("B"), false).unwrap();
        assert_eq!(mode(&path), 0o600);
        rename(&path, "A", "C").unwrap();
        assert_eq!(mode(&path), 0o600);
        delete(&path, "B").unwrap();
        assert_eq!(mode(&path), 0o600);
        let names: Vec<_> = fs::read_dir(&folder)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [FILE_NAME]);

        // Even over a file that was left readable by others.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        put(&path, session("D"), false).unwrap();
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn the_file_holds_only_what_the_types_hold() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let mut saved = session("S");
        saved.tabs[0].root = Node::Pane(PaneRecord {
            kind: Kind::Profile {
                id: ID.to_owned(),
                name: "web".to_owned(),
            },
            name: Some("Front".to_owned()),
            scale: 1.0,
            transcript: true,
        });
        put(&path, saved, false).unwrap();
        let value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "version": 1,
                "sessions": [{
                    "name": "S",
                    "tabs": [{
                        "focused": 0,
                        "root": {
                            "pane": "profile",
                            "profile": ID,
                            "profile_name": "web",
                            "name": "Front",
                            "scale": 1.0,
                            "transcript": true,
                        },
                    }],
                }],
            })
        );
    }

    #[test]
    fn the_next_default_name() {
        assert_eq!(next_default_name(&[]), "Session 1");
        assert_eq!(
            next_default_name(&[session("Session 1"), session("Session 3")]),
            "Session 2"
        );
        assert_eq!(next_default_name(&[session("Session 2")]), "Session 1");
        assert_eq!(
            next_default_name(&[session("Session 1"), session("Session 2")]),
            "Session 3"
        );
        assert_eq!(
            next_default_name(&[session("Work"), session("session 1"), session("Session 01")]),
            "Session 1"
        );
    }
}
