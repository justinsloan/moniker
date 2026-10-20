//! The GUI's own settings: which saved profile is the sync partner (the other
//! machine automatic sync and Sync Now talk to), whether to sync
//! automatically on launch, the text size the next launch starts at, and the
//! Preferences (terminal font, scrollback lines, whether to save a transcript of
//! every session, and the transcripts folder).
//!
//! They live in `gui-settings.json` next to `profiles.json` ([`default_path`]),
//! not in it: they describe this machine only, so they are never synced. The
//! file holds no secret, only a profile id and a switch, and is still written
//! the way the profile file is (mode 0600, a temporary file in the same
//! directory, fsync, rename), so a reader never sees half a file.
//!
//! A missing file is the default; an unreadable or invalid one is the default
//! too (with a line on stderr), so a damaged settings file can turn automatic
//! sync off but never stop the app from starting. A file that is a JSON object
//! is read field by field: a field that is missing, null or of the wrong type
//! is that field's default and costs the other fields nothing.
//!
//! The Preferences are read the same way, and a value that makes no sense is
//! not an error: a scrollback outside its range is brought to the nearest end,
//! a font string that is empty or too long is no font (the system monospace
//! font), and a transcripts folder that is not an absolute path is no folder
//! (the default one).
//!
//! Nothing writes over a file that exists but could not be read or parsed as a
//! JSON object. [`update`], which a change to one setting goes through, and the
//! text size saved when the last window closes ([`save_font_scale`], which is
//! one) leave such a file as it is, so quitting never turns a file the person
//! may want to repair (or that a permission problem hides) into the defaults.
//! (The writes the person asks for with [`save`], flipping the sync switch or a
//! first sync from the sync page, still replace it.)

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use moniker_core::store::ProfileStore;
use serde::Serialize;

use crate::zoom;

/// The settings file's name, beside `profiles.json`.
pub const FILE_NAME: &str = "gui-settings.json";

/// What the settings file holds.
///
/// `Default` is written by hand: automatic sync is on by default (it still
/// does nothing until a partner is recorded, which only a successful sync
/// started by hand on the sync page does). Reading is [`GuiSettings::from_json`],
/// not derived: see there.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct GuiSettings {
    /// Profile id of the other machine, recorded by the first successful sync
    /// from the sync page. It may name a profile that has since been deleted;
    /// readers treat that as no partner.
    pub sync_partner: Option<String>,
    /// Sync with the partner on launch.
    pub auto_sync: bool,
    /// The text size (VTE's font scale) of the tab that was active when the
    /// last window closed: what the next launch's first tab starts at. A file
    /// without it (an older one) means the normal size, and a value outside
    /// `zoom::MIN..=zoom::MAX` is brought into range.
    pub font_scale: f64,
    /// Save a transcript of every new tab, whatever its profile says (the
    /// Preferences switch, off by default).
    pub save_all_transcripts: bool,
    /// Lines of scrollback an ordinary tab keeps (tabs that save a transcript
    /// keep all of theirs). Always within
    /// [`SCROLLBACK_MIN`]`..=`[`SCROLLBACK_MAX`].
    pub scrollback_lines: i64,
    /// The terminal font as a Pango font description string (`"Monospace 11"`);
    /// `None` is the system monospace font. Never empty, never longer than
    /// [`MAX_FONT_CHARS`].
    pub font: Option<String>,
    /// Where transcripts are saved and listed; `None` is
    /// `moniker_core::transcript::default_dir()`. Always an absolute path. The
    /// file is JSON, so only a path that is valid UTF-8 can be saved (saving
    /// any other fails).
    pub transcripts_dir: Option<PathBuf>,
}

/// The scrollback of an ordinary tab unless the person changes it.
pub const SCROLLBACK_DEFAULT: i64 = 10_000;
/// The fewest and the most scrollback lines the settings accept.
pub const SCROLLBACK_MIN: i64 = 1_000;
pub const SCROLLBACK_MAX: i64 = 1_000_000;
/// The longest font description string kept, in characters.
pub const MAX_FONT_CHARS: usize = 200;

impl GuiSettings {
    /// The settings in `text`, which must be a JSON object (anything else is an
    /// error). Each field is read on its own: one that is missing, null or of
    /// the wrong type (`"font_scale": "big"`, `"sync_partner": 7`) is that
    /// field's default and does not cost the others, so a hand-edit slip in one
    /// setting never loses another. The text size is brought into range
    /// ([`zoom::clamp`]): a hand-edited or damaged file never makes a terminal
    /// unreadably small or huge.
    fn from_json(text: &str) -> Result<GuiSettings, serde_json::Error> {
        use serde::de::Error;
        use serde_json::Value;
        let value: Value = serde_json::from_str(text)?;
        let fields = value
            .as_object()
            .ok_or_else(|| serde_json::Error::custom("not a JSON object"))?;
        let defaults = GuiSettings::default();
        Ok(GuiSettings {
            sync_partner: fields
                .get("sync_partner")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            auto_sync: fields
                .get("auto_sync")
                .and_then(Value::as_bool)
                .unwrap_or(defaults.auto_sync),
            font_scale: fields
                .get("font_scale")
                .and_then(Value::as_f64)
                .map_or(defaults.font_scale, zoom::clamp),
            save_all_transcripts: fields
                .get("save_all_transcripts")
                .and_then(Value::as_bool)
                .unwrap_or(defaults.save_all_transcripts),
            scrollback_lines: fields
                .get("scrollback_lines")
                .and_then(Value::as_f64)
                .map_or(defaults.scrollback_lines, clamp_scrollback),
            font: fields
                .get("font")
                .and_then(Value::as_str)
                .filter(|font| is_font_string(font))
                .map(str::to_owned),
            transcripts_dir: fields
                .get("transcripts_dir")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .filter(|dir| dir.is_absolute()),
        })
    }
}

/// `lines` as a scrollback size within [`SCROLLBACK_MIN`]`..=`[`SCROLLBACK_MAX`]:
/// the nearest end for a number outside it (a float is cut to a whole number).
/// JSON has no NaN, but a NaN would be the default.
fn clamp_scrollback(lines: f64) -> i64 {
    if lines.is_nan() {
        return SCROLLBACK_DEFAULT;
    }
    // Both ends are whole numbers an f64 holds exactly, so the cast cannot
    // leave the range.
    lines.clamp(SCROLLBACK_MIN as f64, SCROLLBACK_MAX as f64) as i64
}

/// A usable font description string: not blank and at most [`MAX_FONT_CHARS`]
/// characters. Whether Pango makes a real font of it is for the terminal to
/// find out; a string that names no font there falls back to a default family.
fn is_font_string(font: &str) -> bool {
    !font.trim().is_empty() && font.chars().count() <= MAX_FONT_CHARS
}

impl Default for GuiSettings {
    fn default() -> Self {
        GuiSettings {
            sync_partner: None,
            auto_sync: true,
            font_scale: zoom::DEFAULT,
            save_all_transcripts: false,
            scrollback_lines: SCROLLBACK_DEFAULT,
            font: None,
            transcripts_dir: None,
        }
    }
}

/// `gui-settings.json` in the directory of the profile file.
pub fn default_path() -> PathBuf {
    ProfileStore::default_path().with_file_name(FILE_NAME)
}

/// The settings file at `path`: `None` if there is none, the settings it holds
/// otherwise, and an error if it exists but cannot be read or is not a JSON
/// object (including text that is not UTF-8).
fn read(path: &Path) -> io::Result<Option<GuiSettings>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    GuiSettings::from_json(&text)
        .map(Some)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

/// The settings at `path`. A missing file is the default; a file that cannot
/// be read or parsed is the default too, and stderr says which file it was.
pub fn load(path: &Path) -> GuiSettings {
    match read(path) {
        Ok(settings) => settings.unwrap_or_default(),
        Err(err) => {
            eprintln!(
                "moniker: cannot use {}, using the default settings: {err}",
                path.display()
            );
            GuiSettings::default()
        }
    }
}

/// Write `settings` to `path` so readers see the old file or the new one,
/// never part of one: a temporary file in the same directory, created with
/// mode 0600, fsynced, then renamed over `path`. The directory is created
/// (mode 0700) if it is missing.
pub fn save(path: &Path, settings: &GuiSettings) -> io::Result<()> {
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no directory"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let mut bytes = serde_json::to_vec_pretty(settings).map_err(io::Error::other)?;
    bytes.push(b'\n');

    // A name of its own (pid and a counter), so two Moniker processes saving
    // at once never write the same temporary file. `create_new` never follows
    // or reuses something already there.
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = dir.join(tmp_name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    let written = file.write_all(&bytes).and_then(|()| file.sync_all());
    drop(file);
    let result = written.and_then(|()| fs::rename(&tmp, path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Change the settings in the file at `path` with `change`, keeping everything
/// else as the file has it: read, change, write ([`save`]).
///
/// A missing file starts from the defaults, and nothing is written if `change`
/// leaves the settings as they were (so a first launch that changes nothing
/// leaves no file). Nothing is written, with an error, if the file exists but
/// cannot be read or parsed as a JSON object, and `change` is not called: a
/// damaged file is left as it is for the person to repair, not replaced by the
/// defaults plus one change, and a permission problem that hides the file is
/// never mistaken for "no settings yet".
///
/// This is a read and a rename, not a lock: two processes changing different
/// settings at the same moment can lose one change. (The app changes settings
/// from its one main thread, one at a time.)
pub fn update(path: &Path, change: impl FnOnce(&mut GuiSettings)) -> io::Result<()> {
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

/// Record `scale` (brought into range) as the text size of the next launch,
/// keeping every other setting as the file has it. Nothing is written if the
/// file already says so ([`update`]), so quitting at the usual size creates no
/// file, and nothing is written, with an error, if the file exists but cannot
/// be read or parsed.
pub fn save_font_scale(path: &Path, scale: f64) -> io::Result<()> {
    let scale = zoom::clamp(scale);
    update(path, |settings| settings.font_scale = scale)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const ID: &str = "0b7c6f4e-2f58-4c1b-9a51-3a1d9f3c2e10";

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn settings_round_trip_with_mode_0600() {
        let dir = tempfile::tempdir().unwrap();
        // The config directory may not exist yet: no profile was ever saved.
        let path = dir.path().join("cfg").join("moniker").join(FILE_NAME);
        let settings = GuiSettings {
            sync_partner: Some(ID.to_owned()),
            auto_sync: false,
            font_scale: 1.5,
            save_all_transcripts: true,
            scrollback_lines: 250_000,
            font: Some("Monospace Bold 12".to_owned()),
            transcripts_dir: Some(PathBuf::from("/srv/transcripts")),
        };
        save(&path, &settings).unwrap();
        assert_eq!(load(&path), settings);
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);

        // Saving again replaces the file (still 0600) and leaves no temporary
        // file behind.
        let other = GuiSettings::default();
        save(&path, &other).unwrap();
        assert_eq!(load(&path), other);
        assert_eq!(mode(&path), 0o600);
        let names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [FILE_NAME], "only the settings file is left");

        // A save that cannot rename leaves the old file and no temporary one.
        let blocked = dir.path().join("blocked");
        fs::create_dir_all(blocked.join(FILE_NAME)).unwrap();
        assert!(save(&blocked.join(FILE_NAME), &settings).is_err());
        assert_eq!(fs::read_dir(&blocked).unwrap().count(), 1);
    }

    #[test]
    fn missing_or_corrupt_settings_are_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        assert_eq!(load(&path), GuiSettings::default(), "missing");
        for bad in ["", "{ not json", "[1, 2]", r#"{"sync_partner": 7}"#] {
            fs::write(&path, bad).unwrap();
            assert_eq!(load(&path), GuiSettings::default(), "{bad:?}");
        }
        // A directory where the file should be cannot be read.
        let as_dir = dir.path().join("dir");
        fs::create_dir(&as_dir).unwrap();
        assert_eq!(load(&as_dir), GuiSettings::default());
        // The default: no partner, so nothing syncs on its own.
        assert_eq!(GuiSettings::default().sync_partner, None);
    }

    #[test]
    fn auto_sync_defaults_on_when_absent_from_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        fs::write(&path, format!(r#"{{"sync_partner": "{ID}"}}"#)).unwrap();
        assert_eq!(
            load(&path),
            GuiSettings {
                sync_partner: Some(ID.to_owned()),
                ..GuiSettings::default()
            }
        );
        fs::write(&path, "{}").unwrap();
        assert_eq!(load(&path), GuiSettings::default());
        assert!(GuiSettings::default().auto_sync);
    }

    #[test]
    fn old_settings_files_get_the_default_scale() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        // A file written before text size existed.
        fs::write(
            &path,
            format!(r#"{{"sync_partner": "{ID}", "auto_sync": false}}"#),
        )
        .unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.font_scale, zoom::DEFAULT);
        assert_eq!(
            loaded.sync_partner.as_deref(),
            Some(ID),
            "nothing else lost"
        );
        assert!(!loaded.auto_sync);
        assert_eq!(GuiSettings::default().font_scale, zoom::DEFAULT);
    }

    #[test]
    fn a_saved_scale_is_clamped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let scale = |json: &str| {
            fs::write(&path, json).unwrap();
            load(&path).font_scale
        };
        // Out of range: the nearest end.
        assert_eq!(scale(r#"{"font_scale": -1.0}"#), zoom::MIN);
        assert_eq!(scale(r#"{"font_scale": 0}"#), zoom::MIN);
        assert_eq!(scale(r#"{"font_scale": 100.0}"#), zoom::MAX);
        assert_eq!(scale(r#"{"font_scale": 1.3}"#), 1.3, "in range: as saved");
        // JSON has no NaN, so a file with one is not valid and is the default
        // (like any other damaged file); a value of the wrong type is the
        // normal size, and the other settings stay (`a_bad_field_...`).
        assert_eq!(scale(r#"{"font_scale": NaN}"#), zoom::DEFAULT);
        assert_eq!(scale(r#"{"font_scale": "big"}"#), zoom::DEFAULT);
        assert_eq!(scale(r#"{"font_scale": null}"#), zoom::DEFAULT);
        assert_eq!(scale(r#"{"font_scale": 1e999}"#), zoom::DEFAULT);
    }

    #[test]
    fn a_bad_field_does_not_cost_the_others() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let load_with = |partner: &str, auto_sync: &str, font_scale: &str| {
            fs::write(
                &path,
                format!(
                    r#"{{"sync_partner": {partner}, "auto_sync": {auto_sync}, "font_scale": {font_scale}}}"#
                ),
            )
            .unwrap();
            load(&path)
        };
        let id = format!("\"{ID}\"");
        // The text size is wrong: the partner and the switch are kept.
        for bad in [r#""big""#, "null", "true", "[]", "{}", r#"{"x": 1}"#] {
            assert_eq!(
                load_with(&id, "false", bad),
                GuiSettings {
                    sync_partner: Some(ID.to_owned()),
                    auto_sync: false,
                    ..GuiSettings::default()
                },
                "font_scale {bad}"
            );
        }
        // The partner is wrong.
        for bad in ["7", "null", "false", "[]", "{}"] {
            assert_eq!(
                load_with(bad, "false", "1.5"),
                GuiSettings {
                    sync_partner: None,
                    auto_sync: false,
                    font_scale: 1.5,
                    ..GuiSettings::default()
                },
                "sync_partner {bad}"
            );
        }
        // The switch is wrong: it is on, the default.
        for bad in [r#""no""#, "0", "null", "[]"] {
            assert_eq!(
                load_with(&id, bad, "1.5"),
                GuiSettings {
                    sync_partner: Some(ID.to_owned()),
                    auto_sync: true,
                    font_scale: 1.5,
                    ..GuiSettings::default()
                },
                "auto_sync {bad}"
            );
        }
        // And a size out of range is brought into it, whatever the others are.
        assert_eq!(load_with("7", "null", "100.0").font_scale, zoom::MAX);
    }

    #[test]
    fn saving_the_scale_keeps_the_other_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let before = GuiSettings {
            sync_partner: Some(ID.to_owned()),
            auto_sync: false,
            font_scale: 1.0,
            save_all_transcripts: true,
            scrollback_lines: 5_000,
            font: Some("Monospace 11".to_owned()),
            transcripts_dir: Some(PathBuf::from("/srv/transcripts")),
        };
        save(&path, &before).unwrap();
        save_font_scale(&path, 1.5).unwrap();
        assert_eq!(
            load(&path),
            GuiSettings {
                font_scale: 1.5,
                ..before
            }
        );
        assert_eq!(mode(&path), 0o600);
        // Out of range is brought into range, as a file read does.
        save_font_scale(&path, 100.0).unwrap();
        assert_eq!(load(&path).font_scale, zoom::MAX);
        save_font_scale(&path, f64::NAN).unwrap();
        assert_eq!(load(&path).font_scale, zoom::DEFAULT);
    }

    #[test]
    fn saving_the_scale_the_file_already_has_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        // No file means the normal size: quitting at it leaves no file.
        save_font_scale(&path, zoom::DEFAULT).unwrap();
        assert!(!path.exists());
        save_font_scale(&path, 1.2).unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        save_font_scale(&path, 1.2).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
    }

    #[test]
    fn saving_the_scale_over_a_wrong_typed_field_keeps_the_other_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        fs::write(
            &path,
            format!(r#"{{"sync_partner": "{ID}", "auto_sync": false, "font_scale": "big"}}"#),
        )
        .unwrap();
        save_font_scale(&path, 1.5).unwrap();
        assert_eq!(
            load(&path),
            GuiSettings {
                sync_partner: Some(ID.to_owned()),
                auto_sync: false,
                font_scale: 1.5,
                ..GuiSettings::default()
            }
        );
    }

    #[test]
    fn a_missing_file_is_written_by_saving_the_scale() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfg").join(FILE_NAME);
        save_font_scale(&path, 1.5).unwrap();
        assert_eq!(
            load(&path),
            GuiSettings {
                font_scale: 1.5,
                ..GuiSettings::default()
            }
        );
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn saving_the_scale_leaves_an_unparseable_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let damaged: [&[u8]; 6] = [
            b"{ not json",
            b"",
            b"[1, 2]",
            b"\"just a string\"",
            b"{\"font_scale\": NaN}",
            // Not UTF-8.
            b"{\"sync_partner\": \"\xff\xfe\"}",
        ];
        for bytes in damaged {
            fs::write(&path, bytes).unwrap();
            let err = save_font_scale(&path, 1.5).unwrap_err();
            assert_eq!(fs::read(&path).unwrap(), bytes, "{bytes:?} was rewritten");
            assert!(
                err.to_string().contains("left as it is"),
                "the error says so: {err}"
            );
            // Reading it is still the defaults, and still does not write.
            assert_eq!(load(&path), GuiSettings::default());
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
        // The same size as the default would have written nothing anyway, but
        // it is still told that the file is unusable.
        fs::write(&path, "{ not json").unwrap();
        assert!(save_font_scale(&path, zoom::DEFAULT).is_err());
        // Only the settings file is in the directory: no temporary file.
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn saving_the_scale_leaves_an_unreadable_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let original = format!(r#"{{"sync_partner": "{ID}", "auto_sync": false}}"#);
        fs::write(&path, &original).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        // The superuser reads it anyway, and there is nothing to test.
        if fs::read(&path).is_ok() {
            return;
        }
        let err = save_font_scale(&path, 1.5).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(mode(&path), 0o000, "the file was replaced");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn saving_the_scale_leaves_a_directory_in_the_files_place_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        fs::create_dir(&path).unwrap();
        assert!(save_font_scale(&path, 1.5).is_err());
        assert!(path.is_dir(), "still a directory");
    }

    #[test]
    fn new_fields_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        // Absent from a file written by an earlier version, and from no file.
        for text in ["{}", &format!(r#"{{"sync_partner": "{ID}"}}"#)] {
            fs::write(&path, text).unwrap();
            let loaded = load(&path);
            assert!(!loaded.save_all_transcripts, "{text}");
            assert_eq!(loaded.scrollback_lines, 10_000, "{text}");
            assert_eq!(loaded.font, None, "{text}");
            assert_eq!(loaded.transcripts_dir, None, "{text}");
        }
        let default = GuiSettings::default();
        assert!(!default.save_all_transcripts);
        assert_eq!(default.scrollback_lines, 10_000);
        assert_eq!(default.font, None);
        assert_eq!(default.transcripts_dir, None);
        assert_eq!(load(&dir.path().join("none.json")), default);
    }

    #[test]
    fn good_preference_values_are_read_as_they_are() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let font = "x".repeat(MAX_FONT_CHARS);
        fs::write(
            &path,
            format!(
                r#"{{"save_all_transcripts": true, "scrollback_lines": 1000,
                    "font": "{font}", "transcripts_dir": "/srv/t"}}"#
            ),
        )
        .unwrap();
        let loaded = load(&path);
        assert!(loaded.save_all_transcripts);
        assert_eq!(loaded.scrollback_lines, 1_000);
        assert_eq!(
            loaded.font.as_deref(),
            Some(font.as_str()),
            "200 is allowed"
        );
        assert_eq!(loaded.transcripts_dir, Some(PathBuf::from("/srv/t")));
        for (text, want) in [
            ("1000", 1_000),
            ("1000000", 1_000_000),
            ("2500.9", 2_500),
            ("12345", 12_345),
        ] {
            fs::write(&path, format!(r#"{{"scrollback_lines": {text}}}"#)).unwrap();
            assert_eq!(load(&path).scrollback_lines, want, "{text}");
        }
    }

    #[test]
    fn odd_preference_values_fall_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let long_font = format!("\"{}\"", "f".repeat(300));
        let just_over = format!("\"{}\"", "f".repeat(MAX_FONT_CHARS + 1));
        // Whatever one preference holds, the others (and the older settings)
        // are read as they are.
        let with = |field: &str, value: &str| {
            fs::write(
                &path,
                format!(
                    r#"{{"sync_partner": "{ID}", "auto_sync": false, "font_scale": 1.5,
                        "save_all_transcripts": true, "scrollback_lines": 5000,
                        "font": "Monospace 11", "transcripts_dir": "/srv/t",
                        "{field}": {value}}}"#
                ),
            )
            .unwrap();
            load(&path)
        };
        let kept = |loaded: &GuiSettings, but: &str| {
            assert_eq!(loaded.sync_partner.as_deref(), Some(ID), "{but}");
            assert!(!loaded.auto_sync, "{but}");
            assert_eq!(loaded.font_scale, 1.5, "{but}");
            if but != "save_all_transcripts" {
                assert!(loaded.save_all_transcripts, "{but}");
            }
            if but != "scrollback_lines" {
                assert_eq!(loaded.scrollback_lines, 5_000, "{but}");
            }
            if but != "font" {
                assert_eq!(loaded.font.as_deref(), Some("Monospace 11"), "{but}");
            }
            if but != "transcripts_dir" {
                assert_eq!(
                    loaded.transcripts_dir,
                    Some(PathBuf::from("/srv/t")),
                    "{but}"
                );
            }
        };

        for (value, want) in [
            ("0", 1_000),
            ("-5", 1_000),
            ("999", 1_000),
            ("1e12", 1_000_000),
            ("1000001", 1_000_000),
            ("99999999999999999999", 1_000_000),
            ("-1e12", 1_000),
            ("0.4", 1_000),
            // The wrong type is the default, not the nearest end.
            (r#""lots""#, SCROLLBACK_DEFAULT),
            ("null", SCROLLBACK_DEFAULT),
            ("true", SCROLLBACK_DEFAULT),
            ("[]", SCROLLBACK_DEFAULT),
            ("{}", SCROLLBACK_DEFAULT),
        ] {
            let loaded = with("scrollback_lines", value);
            assert_eq!(loaded.scrollback_lines, want, "scrollback_lines {value}");
            kept(&loaded, "scrollback_lines");
        }

        for value in [
            r#""""#,
            r#""   ""#,
            long_font.as_str(),
            just_over.as_str(),
            "5",
            "null",
            "true",
            "[]",
            r#"{"family": "Mono"}"#,
        ] {
            let loaded = with("font", value);
            assert_eq!(loaded.font, None, "font {value}");
            kept(&loaded, "font");
        }

        for value in [
            r#""relative/x""#,
            r#""""#,
            r#""~/transcripts""#,
            r#""./here""#,
            "5",
            "null",
            "false",
            "[]",
        ] {
            let loaded = with("transcripts_dir", value);
            assert_eq!(loaded.transcripts_dir, None, "transcripts_dir {value}");
            kept(&loaded, "transcripts_dir");
        }

        for value in [r#""yes""#, "1", "null", "[]"] {
            let loaded = with("save_all_transcripts", value);
            assert!(!loaded.save_all_transcripts, "save_all_transcripts {value}");
            kept(&loaded, "save_all_transcripts");
        }
    }

    #[test]
    fn update_writes_one_field_and_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let before = GuiSettings {
            sync_partner: Some(ID.to_owned()),
            auto_sync: false,
            font_scale: 1.5,
            save_all_transcripts: false,
            scrollback_lines: 20_000,
            font: Some("Monospace 11".to_owned()),
            transcripts_dir: Some(PathBuf::from("/srv/t")),
        };
        save(&path, &before).unwrap();

        update(&path, |s| s.save_all_transcripts = true).unwrap();
        assert_eq!(
            load(&path),
            GuiSettings {
                save_all_transcripts: true,
                ..before.clone()
            }
        );
        assert_eq!(mode(&path), 0o600);

        update(&path, |s| s.font = None).unwrap();
        update(&path, |s| s.transcripts_dir = Some(PathBuf::from("/other"))).unwrap();
        assert_eq!(
            load(&path),
            GuiSettings {
                save_all_transcripts: true,
                font: None,
                transcripts_dir: Some(PathBuf::from("/other")),
                ..before
            }
        );
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [FILE_NAME], "no temporary file is left");
    }

    #[test]
    fn update_over_a_wrong_typed_field_keeps_the_other_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        fs::write(
            &path,
            format!(r#"{{"sync_partner": "{ID}", "auto_sync": false, "font": 5}}"#),
        )
        .unwrap();
        update(&path, |s| s.scrollback_lines = 3_000).unwrap();
        assert_eq!(
            load(&path),
            GuiSettings {
                sync_partner: Some(ID.to_owned()),
                auto_sync: false,
                scrollback_lines: 3_000,
                ..GuiSettings::default()
            }
        );
    }

    #[test]
    fn update_that_changes_nothing_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        update(&path, |_| {}).unwrap();
        update(&path, |s| s.auto_sync = true).unwrap();
        assert!(!path.exists(), "the default needs no file");
        update(&path, |s| s.scrollback_lines = 2_000).unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        update(&path, |s| s.scrollback_lines = 2_000).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
    }

    #[test]
    fn update_refuses_a_damaged_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let damaged: [&[u8]; 4] = [b"{ not json", b"", b"[1, 2]", b"{\"font\": \"\xff\xfe\"}"];
        for bytes in damaged {
            fs::write(&path, bytes).unwrap();
            let mut called = false;
            let err = update(&path, |s| {
                called = true;
                s.save_all_transcripts = true;
            })
            .unwrap_err();
            assert!(!called, "nothing is changed before the file is known good");
            assert!(
                err.to_string().contains("left as it is"),
                "the error says so: {err}"
            );
            assert_eq!(fs::read(&path).unwrap(), bytes, "{bytes:?} was rewritten");
        }
        // A file nobody may read is left alone too (the superuser reads it).
        let original = r#"{"auto_sync": false}"#;
        fs::write(&path, original).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&path).is_err() {
            let err = update(&path, |s| s.save_all_transcripts = true).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
            assert_eq!(mode(&path), 0o000, "the file was replaced");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), original);
        }
        // Only the settings file is in the directory: no temporary file.
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn update_creates_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfg").join("moniker").join(FILE_NAME);
        update(&path, |s| s.save_all_transcripts = true).unwrap();
        assert_eq!(
            load(&path),
            GuiSettings {
                save_all_transcripts: true,
                ..GuiSettings::default()
            }
        );
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
    }

    #[test]
    fn the_file_sits_beside_the_profile_file() {
        assert_eq!(
            default_path(),
            ProfileStore::default_path().with_file_name("gui-settings.json")
        );
    }
}
