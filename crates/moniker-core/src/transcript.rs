//! Session transcript files: where they go and how they are written.
//!
//! A transcript is the plain text of one terminal session, written when its
//! tab closes. Transcripts stay on the machine that recorded them (they are
//! never synced) and can contain anything the user typed or saw, so the files
//! are mode 0600 and every directory created for them is mode 0700.
//!
//! The file name is built from the host and the start time:
//! `<host>_YYYYMMDD-HHMMSS.txt` (UTC). The host comes from a profile, which can
//! arrive from another machine, so [`transcript_path`] reduces it to a short
//! name in `[A-Za-z0-9._-]` that can never leave the transcript directory.
//! [`write_transcript`] never overwrites or follows a link: if the name is
//! taken it appends `-2`, `-3`, ... before the extension.
//!
//! [`list_recordings`] reads the directory back for the Transcripts
//! window. [`parse_file_name`] undoes the naming above to get the host and the
//! start time, and only files whose names it recognizes are listed: the folder
//! can be one the person also uses for other things (their home folder,
//! Downloads), and the window reads, searches, exports and deletes what it
//! lists. Only regular files directly in the directory are listed, and links
//! are never followed: the directory is a place other programs and the user can
//! write to, and a link in it must not make the window read (or later delete or
//! export) something elsewhere.
//!
//! [`move_transcripts`] carries the transcripts to another folder when the
//! person chooses one. It moves only regular files directly in the old folder
//! whose names [`parse_file_name`] recognizes (the names Moniker gives
//! transcripts), with the same no-follow rules as [`list_recordings`], and never
//! replaces a file in the new folder. Everything else in the old folder stays
//! where it is: the person may once have chosen their home folder or Documents
//! as the transcripts folder, and a move must not carry off whatever else lives
//! there.

use std::ffi::OsStr;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use rustix::fs::{Access, CWD, Mode, OFlags, RenameFlags};
use rustix::io::Errno;

use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};

/// Longest host part of a file name, in characters (all ASCII after cleaning).
const MAX_HOST_CHARS: usize = 64;

/// Host part used when the host is empty.
const UNKNOWN_HOST: &str = "unknown-host";

/// How many files with the same name [`write_transcript`] tries (the name
/// itself, then `-2` up to this number) before giving up.
const MAX_ATTEMPTS: u32 = 1000;

/// `$XDG_DATA_HOME/moniker/transcripts`, or `$HOME/.local/share/moniker/transcripts`
/// when `XDG_DATA_HOME` is unset, empty or not an absolute path.
pub fn default_dir() -> PathBuf {
    data_dir_from(
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// The transcript directory given the values of `XDG_DATA_HOME` and `HOME`.
///
/// `<xdg>/moniker/transcripts` if `xdg` is an absolute path (the XDG spec says
/// to ignore empty and relative values), else
/// `<home>/.local/share/moniker/transcripts`. With neither, the path is relative
/// to the working directory (`.local/share/moniker/transcripts`).
fn data_dir_from(xdg: Option<&OsStr>, home: Option<&OsStr>) -> PathBuf {
    let base = match (xdg, home) {
        (Some(x), _) if Path::new(x).is_absolute() => PathBuf::from(x),
        (_, Some(h)) if !h.is_empty() => Path::new(h).join(".local").join("share"),
        _ => PathBuf::from(".local/share"),
    };
    base.join("moniker").join("transcripts")
}

/// `dir/<host>_YYYYMMDD-HHMMSS.txt` for a session with `host` that started at
/// `unix_secs` (seconds since the epoch, shown in UTC).
///
/// The host is cleaned so the result is always one file name directly in `dir`:
/// each character outside `[A-Za-z0-9._-]` becomes `_`, the result is cut to
/// 64 characters, a leading `.` becomes `_`, and an empty host becomes
/// `unknown-host`. A time outside years 0 to 9999 is shown as the epoch.
pub fn transcript_path(dir: &Path, host: &str, unix_secs: i64) -> PathBuf {
    let when = OffsetDateTime::from_unix_timestamp(unix_secs)
        .ok()
        .filter(|t| (0..=9999).contains(&t.year()))
        .unwrap_or(OffsetDateTime::UNIX_EPOCH);
    dir.join(format!(
        "{}_{:04}{:02}{:02}-{:02}{:02}{:02}.txt",
        clean_host(host),
        when.year(),
        u8::from(when.month()),
        when.day(),
        when.hour(),
        when.minute(),
        when.second(),
    ))
}

/// The host part of a transcript file name. See [`transcript_path`].
fn clean_host(host: &str) -> String {
    let mut name: String = host
        .chars()
        .take(MAX_HOST_CHARS)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if name.is_empty() {
        return UNKNOWN_HOST.to_owned();
    }
    if name.starts_with('.') {
        name.replace_range(..1, "_");
    }
    name
}

/// Write `text` to a new file at `path` and return the path actually written.
///
/// Missing parent directories are created with mode 0700 (existing ones are
/// left as they are). The file is created exclusively with mode 0600 from the
/// start, so an existing file or link is never overwritten or followed and
/// the content is never briefly readable by others. If `path` is taken, the
/// next free name among `<stem>-2.<ext>`, `<stem>-3.<ext>`, ... is used, up to
/// 1000 attempts, then it is an `AlreadyExists` error. If writing fails, the
/// file just created is removed again.
pub fn write_transcript(path: &Path, text: &str) -> io::Result<PathBuf> {
    if path.file_name().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "transcript path has no file name",
        ));
    }
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)?;

    let (mut file, written) = create_unique(path)?;
    if let Err(err) = file.write_all(text.as_bytes()) {
        drop(file);
        let _ = fs::remove_file(&written);
        return Err(err);
    }
    Ok(written)
}

/// Create a new file for writing at `path`, or at the next free numbered name
/// (see [`attempt_path`]), and return it with the path actually used.
///
/// The file is created exclusively with mode 0600 from the start, so an
/// existing file or link (even a dangling one) is never opened or followed and
/// the content is never briefly readable by others. The parent directory must
/// exist. Gives up after [`MAX_ATTEMPTS`] names with `AlreadyExists`.
fn create_unique(path: &Path) -> io::Result<(File, PathBuf)> {
    for attempt in 1..=MAX_ATTEMPTS {
        let candidate = attempt_path(path, attempt);
        let created = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&candidate);
        match created {
            Ok(file) => return Ok((file, candidate)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err),
        }
    }
    Err(too_many_with_the_same_name())
}

fn too_many_with_the_same_name() -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        "too many transcripts with the same name",
    )
}

/// The name to try on attempt `attempt` (counting from 1): `path` itself, then
/// `<stem>-2.<ext>`, `<stem>-3.<ext>`, ...
fn attempt_path(path: &Path, attempt: u32) -> PathBuf {
    if attempt == 1 {
        path.to_path_buf()
    } else {
        numbered(path, attempt)
    }
}

/// `path` with `-<n>` added to the file stem: `a_1.txt` -> `a_1-2.txt`,
/// `log` -> `log-2`. The path has a file name (checked by the caller).
fn numbered(path: &Path, n: u32) -> PathBuf {
    let mut name = path.file_stem().unwrap_or_default().to_os_string();
    name.push(format!("-{n}"));
    if let Some(ext) = path.extension() {
        name.push(".");
        name.push(ext);
    }
    path.with_file_name(name)
}

/// The host and the start time (Unix seconds, UTC) in a transcript file name,
/// the inverse of [`transcript_path`]; `None` for any other name.
///
/// The accepted forms are `<host>_<YYYYMMDD>-<HHMMSS>.txt` and the collision
/// form [`write_transcript`] falls back to, `<host>_<YYYYMMDD>-<HHMMSS>-<n>.txt`.
/// The host is everything before the last `_` (a host can hold underscores, the
/// stamp cannot) and must not be empty. The stamp must be a real date and time:
/// digits only, a month 1 to 12, a day that exists in that month, a second up
/// to 59 (no leap seconds, which [`transcript_path`] never writes either).
/// Hosts come back as they are in the name, i.e. after cleaning, so a host
/// that cleaning changed (`høst`) comes back as `h_st`.
pub fn parse_file_name(name: &str) -> Option<(String, i64)> {
    let stem = name.strip_suffix(".txt")?;
    let (host, stamp) = stem.rsplit_once('_')?;
    if host.is_empty() {
        return None;
    }
    let mut parts = stamp.splitn(3, '-');
    let (date, time) = (parts.next()?, parts.next()?);
    if let Some(n) = parts.next() {
        // The collision number: digits only (no sign), and one that fits.
        if !all_digits(n) || n.parse::<u32>().is_err() {
            return None;
        }
    }
    if date.len() != 8 || time.len() != 6 || !all_digits(date) || !all_digits(time) {
        return None;
    }
    // Every slice below is ASCII digits (checked above), so slicing by bytes is
    // safe and the parses only fail on a value out of range.
    let date = Date::from_calendar_date(
        date[..4].parse().ok()?,
        Month::try_from(date[4..6].parse::<u8>().ok()?).ok()?,
        date[6..].parse().ok()?,
    )
    .ok()?;
    let time = Time::from_hms(
        time[..2].parse().ok()?,
        time[2..4].parse().ok()?,
        time[4..].parse().ok()?,
    )
    .ok()?;
    Some((
        host.to_owned(),
        PrimitiveDateTime::new(date, time)
            .assume_utc()
            .unix_timestamp(),
    ))
}

/// Not empty, and every byte is an ASCII digit (`str::parse` would also take a
/// leading `+`, and the file name has no use for that).
fn all_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// One transcript file, as the Transcripts window lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recording {
    /// The file, directly in the transcript directory.
    pub path: PathBuf,
    /// The host from the file name ([`parse_file_name`]). Text from a file
    /// name: show it as plain text.
    pub host: String,
    /// When the session started, in Unix seconds (UTC), from the file name.
    pub started: i64,
    /// The size of the file in bytes.
    pub bytes: u64,
}

/// The transcripts in `dir`, in file-name order. A `dir` that does not exist
/// yet (nothing has been recorded) is an empty list; any other failure to read
/// it is an error.
///
/// Only Moniker's own transcripts are listed: regular files directly in `dir`
/// whose names [`parse_file_name`] recognizes (the names Moniker gives
/// transcripts, the same rule [`move_transcripts`] follows), with the host and
/// start time read from the name. Every other file is left out, never opened
/// and never shown: the Transcripts window reads, searches, exports and deletes
/// what this lists, and the folder may be one the person also uses for other
/// things (their home folder, Downloads), where `.bashrc` must not be offered
/// for deletion and a film of several gigabytes must not be read whole into
/// memory by a search. A name that is not valid UTF-8 is not one Moniker gives,
/// and a transcript the person renamed is no longer listed (it stays on disk).
///
/// Each entry is examined with `symlink_metadata`, so a symbolic link (to a
/// file, a directory or nothing), a subdirectory, a FIFO, a socket or a device
/// is skipped whatever its name, and a file is never opened here, so nothing
/// blocks or leaves `dir`. A file that disappears while the directory is read
/// (another window deleting it) is skipped too.
pub fn list_recordings(dir: &Path) -> io::Result<Vec<Recording>> {
    let mut recordings = Vec::new();
    for (path, meta) in regular_files(dir)? {
        if let Some((host, started)) = transcript_name(&path) {
            recordings.push(Recording {
                path,
                host,
                started,
                bytes: meta.len(),
            });
        }
    }
    Ok(recordings)
}

/// The host and start time in `path`'s file name if it is a name Moniker gives
/// a transcript ([`parse_file_name`]), else `None`; a name that is not valid
/// UTF-8 is not one.
fn transcript_name(path: &Path) -> Option<(String, i64)> {
    path.file_name()
        .and_then(OsStr::to_str)
        .and_then(parse_file_name)
}

/// The regular files directly in `dir`, with their metadata, in path order; the
/// listing [`list_recordings`] and [`move_transcripts`] both start from. A `dir`
/// that does not exist is an empty list. Links are never followed and nothing is
/// opened (see [`list_recordings`]).
fn regular_files(dir: &Path) -> io::Result<Vec<(PathBuf, fs::Metadata)>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let mut files = Vec::new();
    for entry in entries {
        let path = entry?.path();
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        if meta.file_type().is_file() {
            files.push((path, meta));
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

/// What [`move_transcripts`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MoveReport {
    /// Files now in the new folder (and no longer in the old one).
    pub moved: usize,
    /// Files that could not be moved. Each is still, whole, in the old folder
    /// and has left nothing behind in the new one.
    ///
    /// A file that was removed by something else after it was listed (the
    /// person deleting it in the Transcripts window) is in neither count: it
    /// is not in the new folder, and it is not in the old one either.
    pub failed: usize,
    /// Regular files in the old folder that were left there because their
    /// names are not transcript names ([`parse_file_name`]): `.bashrc`,
    /// `notes.txt`, whatever else lives in a folder the person once chose.
    /// Links, subdirectories and other odd entries are left too, but they are
    /// not files the person might expect to have moved, so they are not counted.
    pub other: usize,
}

/// Make `to` ready to be the transcripts folder: it must be an absolute path
/// (`InvalidInput` otherwise, and nothing is created); a missing one is created,
/// parents too, with mode 0700; and it must then be a directory this process
/// can write to (`PermissionDenied` if it cannot). [`move_transcripts`] starts
/// with this, and the app calls it before it records a newly chosen folder, so
/// a folder that cannot take transcripts is refused before anything changes.
pub fn prepare_folder(to: &Path) -> io::Result<()> {
    if !to.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the transcripts folder must be an absolute path",
        ));
    }
    DirBuilder::new().recursive(true).mode(0o700).create(to)?;
    rustix::fs::access(to, Access::WRITE_OK | Access::EXEC_OK)?;
    Ok(())
}

/// Move the transcripts in `from` into `to`, for when the person chooses
/// another transcripts folder.
///
/// What moves: regular files directly in `from`, found without following links
/// (as [`list_recordings`] finds them), whose names [`parse_file_name`]
/// recognizes, i.e. `<host>_<YYYYMMDD>-<HHMMSS>.txt` with the optional `-<n>`
/// collision number, the names Moniker gives transcripts. Every other entry
/// stays where it is, untouched: a regular file with another name (`.bashrc`,
/// `notes.txt`) is counted in [`MoveReport::other`] so the person can be told,
/// and a symbolic link, a subdirectory, a FIFO or anything else is neither
/// moved nor counted. So `to` may be a folder inside `from` (it stays, as a
/// subdirectory is not a transcript).
///
/// `to` must be an absolute path (`InvalidInput` otherwise, and nothing is
/// created). A missing `to` is created, parents too, with mode 0700; an
/// existing one must be a directory the process can write to, else this is an
/// error before any file moves, even if there is nothing to move (the folder is
/// where the next transcript will go). Nothing moves, and the result is `Ok`
/// with zero counts, if `from` does not exist or if `from` and `to` are the
/// same directory (same device and inode, so a link to it or a path through
/// `..` counts).
///
/// A file whose name is taken in `to` gets a number (`x_<stamp>.txt` ->
/// `x_<stamp>-2.txt`, then `-3`, ..., as [`write_transcript`] numbers); nothing
/// in `to` is ever replaced or written through. Each file is renamed into place
/// if both folders are on one file system. Otherwise it is copied (mode 0600,
/// the same modification time, flushed to disk with its folder entry) and the
/// original removed only after the copy is complete and the original has been
/// seen unchanged. A file that fails for any reason is counted in
/// [`MoveReport::failed`], the others still move. An error does not carry the
/// content of any file.
///
/// A transcript that something is still writing to in `from` is the caller's
/// to keep out of the way: a copy of a file that changes while it is copied is
/// refused (the original stays, the copy is removed), but that check narrows
/// the race and cannot close it, so the app does not write into the old folder
/// while this runs. Across file systems a copy is written under its final name,
/// so a process that dies in the middle of one leaves a truncated file in `to`
/// (the original is still whole in `from`): the caller keeps the process alive
/// until this returns.
pub fn move_transcripts(from: &Path, to: &Path) -> io::Result<MoveReport> {
    // Better one clear error than a "could not be moved" for every file, and
    // the same error when there is nothing to move: later saves would fail.
    prepare_folder(to)?;

    let source = match fs::metadata(from) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(MoveReport::default()),
        Err(err) => return Err(err),
    };
    let target = fs::metadata(to)?;
    if (source.dev(), source.ino()) == (target.dev(), target.ino()) {
        return Ok(MoveReport::default());
    }

    let mut transcripts = Vec::new();
    let mut other = 0;
    for (path, _) in regular_files(from)? {
        if transcript_name(&path).is_some() {
            transcripts.push(path);
        } else {
            other += 1;
        }
    }
    let mut report = move_files(&transcripts, to);
    report.other = other;
    Ok(report)
}

/// Move each of `files` into `to` and count the results.
///
/// A failure with `NotFound` for a file that no longer exists (it was listed,
/// then removed by something else) is not a failure: the file is gone from the
/// old folder, so "still in the old folder" would be false. A `NotFound` for a
/// file that is still there (`to` vanished) is one.
fn move_files(files: &[PathBuf], to: &Path) -> MoveReport {
    let mut report = MoveReport::default();
    for src in files {
        match move_one(src, to, true) {
            Ok(_) => report.moved += 1,
            Err(err) if err.kind() == io::ErrorKind::NotFound && is_gone(src) => {}
            Err(_) => report.failed += 1,
        }
    }
    report
}

/// There is nothing at `path` (not even a dangling link).
fn is_gone(path: &Path) -> bool {
    matches!(fs::symlink_metadata(path), Err(err) if err.kind() == io::ErrorKind::NotFound)
}

/// Move the one file `src` into the directory `to` under its own name (or a
/// numbered one if that is taken) and return the path it now has.
///
/// With `rename_first` the file is renamed with `RENAME_NOREPLACE`, which fails
/// instead of replacing, so a name taken in `to` (even by a dangling link) goes
/// on to the next number; it is copied instead only when the rename cannot be
/// done across the two places (`EXDEV`, or a file system that does not know
/// the flag). Without it the copy is used straight away, which is how the tests
/// reach that path on a single file system.
///
/// Whichever way, `src` must be a regular file, and a failure leaves `src`
/// whole where it was and nothing in `to`. (If `src` itself is removed by
/// something else meanwhile, the error is `NotFound` and there is nothing left
/// to keep whole.)
pub(crate) fn move_one(src: &Path, to: &Path, rename_first: bool) -> io::Result<PathBuf> {
    let name = src
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
    let target = to.join(name);
    if rename_first {
        // `rename` moves a link itself rather than what it points to, so a
        // link is refused here (the copy checks the file it opened instead).
        if !fs::symlink_metadata(src)?.file_type().is_file() {
            return Err(not_a_regular_file());
        }
        if let Some(moved) = rename_without_replacing(src, &target)? {
            return Ok(moved);
        }
    }
    copy_then_remove(src, &target)
}

fn not_a_regular_file() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "not a regular file")
}

/// Rename `src` to `target`, or to the next free numbered name, never
/// replacing anything. `None` means the rename cannot be done here and the
/// file has to be copied: the folders are on different file systems (`EXDEV`),
/// or the file system does not support `RENAME_NOREPLACE` (`EINVAL`, `ENOSYS`).
fn rename_without_replacing(src: &Path, target: &Path) -> io::Result<Option<PathBuf>> {
    for attempt in 1..=MAX_ATTEMPTS {
        let candidate = attempt_path(target, attempt);
        match rustix::fs::renameat_with(CWD, src, CWD, &candidate, RenameFlags::NOREPLACE) {
            Ok(()) => return Ok(Some(candidate)),
            Err(Errno::EXIST) => {}
            Err(Errno::XDEV | Errno::INVAL | Errno::NOSYS) => return Ok(None),
            Err(err) => return Err(err.into()),
        }
    }
    Err(too_many_with_the_same_name())
}

/// Copy the regular file `src` to a new file at `target` (or the next free
/// numbered name), make the copy durable, and only then remove `src`.
///
/// `src` is opened without following a link and without waiting for a writer
/// (a FIFO opens at once), and what was opened must be a regular file. The copy
/// is mode 0600 and gets the original's modification time (when the session
/// ended, which a file manager shows). The copy and its folder entry are flushed
/// to disk before `src` is removed, so a crash cannot leave the removal on disk
/// without the copy. Just before the removal `src` is looked at once more: the
/// name must still be the file that was opened, with the size and modification
/// time it had when the copy began. Otherwise it was replaced, or something
/// wrote to it while it was copied, and the copy may be stale or incomplete.
///
/// If anything fails, including that check and the removal of `src`, the new
/// file is removed again, so a failure leaves exactly the original (and a
/// replacement file is never removed). The check narrows the race with a
/// writer and cannot close it: a change that keeps the size and falls within
/// one tick of the file system's clock is not seen, so a transcript still being
/// written into the old folder is for the caller to prevent.
fn copy_then_remove(src: &Path, target: &Path) -> io::Result<PathBuf> {
    let fd = rustix::fs::openat(
        CWD,
        src,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let mut input = File::from(fd);
    let before = input.metadata()?;
    if !before.file_type().is_file() {
        return Err(not_a_regular_file());
    }
    let (mut output, copy) = create_unique(target)?;
    let done = copy_and_remove(src, &mut input, &before, &mut output, &copy);
    drop(output);
    match done {
        Ok(()) => Ok(copy),
        Err(err) => {
            let _ = fs::remove_file(&copy);
            Err(err)
        }
    }
}

/// The steps of [`copy_then_remove`] after the new file exists, so that any
/// failure in them is cleaned up in one place.
fn copy_and_remove(
    src: &Path,
    input: &mut File,
    before: &fs::Metadata,
    output: &mut File,
    copy: &Path,
) -> io::Result<()> {
    io::copy(input, output)?;
    output.set_modified(before.modified()?)?;
    output.sync_all()?;
    sync_dir(copy.parent().unwrap_or(Path::new(".")))?;
    #[cfg(test)]
    tests::run_after_copy_hook(src);
    unchanged_since(src, input, before)?;
    fs::remove_file(src)
}

/// Flush the directory `dir` (its entries) to disk.
fn sync_dir(dir: &Path) -> io::Result<()> {
    let fd = rustix::fs::openat(
        CWD,
        dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    File::from(fd).sync_all()
}

/// `Ok` if the name `src` still belongs to the file `input` (opened from it
/// earlier), and `input` still has the size and modification time that `before`
/// recorded. A `src` that is gone is `NotFound`.
fn unchanged_since(src: &Path, input: &File, before: &fs::Metadata) -> io::Result<()> {
    let now = input.metadata()?;
    let named = fs::symlink_metadata(src)?;
    let same_file =
        named.file_type().is_file() && (named.dev(), named.ino()) == (before.dev(), before.ino());
    if same_file && now.len() == before.len() && now.modified()? == before.modified()? {
        Ok(())
    } else {
        Err(io::Error::other(
            "the transcript changed while it was being copied",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use tempfile::TempDir;

    /// What a test runs in the middle of a copy.
    type Hook = Box<dyn FnOnce(&Path)>;

    thread_local! {
        /// The hook set by [`after_the_copy`] on this thread, if not yet run.
        static AFTER_COPY: std::cell::RefCell<Option<Hook>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Called by the copy path after the copy is complete and durable and
    /// before the original is checked and removed: the moment a writer or a
    /// deletion in the old folder would matter. Runs the hook set by the test
    /// on this thread, once.
    pub(super) fn run_after_copy_hook(src: &Path) {
        let hook = AFTER_COPY.with(|hook| hook.borrow_mut().take());
        if let Some(hook) = hook {
            hook(src);
        }
    }

    /// Run `hook` (given the source's path) in the middle of the next copy.
    fn after_the_copy(hook: impl FnOnce(&Path) + 'static) {
        AFTER_COPY.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
    }

    /// 2026-10-16 07:45:00 UTC.
    const WHEN: i64 = 1_792_136_700;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn file_name(path: &Path) -> String {
        path.file_name().unwrap().to_str().unwrap().to_owned()
    }

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn path_format() {
        let path = transcript_path(Path::new("/data/t"), "example.com", WHEN);
        assert_eq!(path, p("/data/t/example.com_20261016-074500.txt"));
    }

    #[test]
    fn path_format_is_utc_and_zero_padded() {
        let dir = Path::new("/d");
        assert_eq!(transcript_path(dir, "h", 0), p("/d/h_19700101-000000.txt"));
        // 2024-02-29 23:59:59 UTC, a leap day.
        assert_eq!(
            transcript_path(dir, "h", 1_709_251_199),
            p("/d/h_20240229-235959.txt")
        );
    }

    #[test]
    fn out_of_range_time_still_gives_a_safe_name() {
        for secs in [i64::MIN, i64::MAX, -62_167_219_201, 253_402_300_800] {
            let path = transcript_path(Path::new("/d"), "h", secs);
            let name = file_name(&path);
            assert_eq!(path.parent(), Some(Path::new("/d")), "{secs}");
            assert!(name.starts_with("h_") && name.ends_with(".txt"), "{name}");
            assert!(
                name.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
                "{name}"
            );
        }
    }

    #[test]
    fn host_characters_are_sanitized() {
        let dir = Path::new("/d");
        let name = |host: &str| file_name(&transcript_path(dir, host, WHEN));
        assert_eq!(name("høst"), "h_st_20261016-074500.txt");
        assert_eq!(name("fe80::1%eth0"), "fe80__1_eth0_20261016-074500.txt");
        assert_eq!(name("a b/c\\d"), "a_b_c_d_20261016-074500.txt");
        assert_eq!(
            name("My-Host_1.example"),
            "My-Host_1.example_20261016-074500.txt"
        );
        assert_eq!(name(".hidden"), "_hidden_20261016-074500.txt");
        assert_eq!(name(""), "unknown-host_20261016-074500.txt");
    }

    #[test]
    fn long_host_is_truncated_to_64_characters() {
        let name = file_name(&transcript_path(Path::new("/d"), &"a".repeat(300), WHEN));
        assert_eq!(name, format!("{}_20261016-074500.txt", "a".repeat(64)));
    }

    #[test]
    fn hostile_hosts_cannot_escape_dir() {
        let dir = Path::new("/data/transcripts");
        let long = "x".repeat(300);
        let hosts = [
            "../../etc/passwd",
            "",
            long.as_str(),
            "høst",
            "..",
            "/abs/path",
            "a\0b",
            "\n",
        ];
        for host in hosts {
            let path = transcript_path(dir, host, WHEN);
            let name = file_name(&path);
            assert!(!name.contains('/'), "{host:?} -> {name}");
            assert_eq!(path.parent(), Some(dir), "{host:?} -> {path:?}");
            assert!(name.len() <= 100, "{host:?} -> {} bytes", name.len());
            assert!(!name.starts_with('.'), "{host:?} -> {name}");
            if host.is_empty() {
                assert!(name.starts_with("unknown-host"), "{name}");
            }
        }
    }

    #[test]
    fn write_creates_file_and_returns_its_path() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("t.txt");
        let written = write_transcript(&path, "hello\nworld\n").unwrap();
        assert_eq!(written, path);
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello\nworld\n");
    }

    #[test]
    fn existing_file_is_not_clobbered() {
        let tmp = TempDir::new().unwrap();
        let path = transcript_path(tmp.path(), "example.com", WHEN);

        let first = write_transcript(&path, "one").unwrap();
        let second = write_transcript(&path, "two").unwrap();
        let third = write_transcript(&path, "three").unwrap();

        assert_eq!(first, path);
        assert_eq!(file_name(&second), "example.com_20261016-074500-2.txt");
        assert_eq!(file_name(&third), "example.com_20261016-074500-3.txt");
        assert_eq!(fs::read_to_string(&first).unwrap(), "one");
        assert_eq!(fs::read_to_string(&second).unwrap(), "two");
        assert_eq!(fs::read_to_string(&third).unwrap(), "three");
    }

    #[test]
    fn collision_suffix_works_without_an_extension() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("log");
        write_transcript(&path, "a").unwrap();
        assert_eq!(file_name(&write_transcript(&path, "b").unwrap()), "log-2");
    }

    #[test]
    fn gives_up_after_many_collisions() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("t.txt");
        fs::write(&path, "x").unwrap();
        for n in 2..=1000 {
            fs::write(tmp.path().join(format!("t-{n}.txt")), "x").unwrap();
        }
        let err = write_transcript(&path, "new").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1000);
    }

    #[test]
    fn a_symlink_at_the_target_is_not_followed() {
        let tmp = TempDir::new().unwrap();
        let victim = tmp.path().join("victim");
        let path = tmp.path().join("t.txt");
        symlink(&victim, &path).unwrap(); // dangling
        let written = write_transcript(&path, "secret").unwrap();
        assert_eq!(file_name(&written), "t-2.txt");
        assert!(!victim.exists(), "the symlink target must not be created");
    }

    #[test]
    fn transcript_is_0600_and_dir_0700() {
        let tmp = TempDir::new().unwrap();
        let outer = tmp.path().join("share");
        let dir = outer.join("transcripts");
        let path = dir.join("t.txt");
        write_transcript(&path, "secret").unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&outer), 0o700, "every directory it creates is private");
        // A second file in the same directory is private too.
        let second = write_transcript(&path, "again").unwrap();
        assert_eq!(mode(&second), 0o600);
    }

    #[test]
    fn existing_directory_permissions_are_left_alone() {
        let tmp = TempDir::new().unwrap();
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o755)).unwrap();
        write_transcript(&tmp.path().join("t.txt"), "x").unwrap();
        assert_eq!(mode(tmp.path()), 0o755);
    }

    #[test]
    fn path_without_a_file_name_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let err = write_transcript(&tmp.path().join(".."), "x").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn data_dir_prefers_xdg_data_home() {
        let got = data_dir_from(
            Some(OsStr::new("/xdg/data")),
            Some(OsStr::new("/home/alice")),
        );
        assert_eq!(got, p("/xdg/data/moniker/transcripts"));
    }

    #[test]
    fn data_dir_falls_back_to_home_local_share() {
        let got = data_dir_from(None, Some(OsStr::new("/home/alice")));
        assert_eq!(got, p("/home/alice/.local/share/moniker/transcripts"));
    }

    #[test]
    fn data_dir_treats_empty_xdg_as_unset() {
        let got = data_dir_from(Some(OsStr::new("")), Some(OsStr::new("/home/alice")));
        assert_eq!(got, p("/home/alice/.local/share/moniker/transcripts"));
    }

    #[test]
    fn data_dir_ignores_relative_xdg() {
        let got = data_dir_from(Some(OsStr::new("rel/dir")), Some(OsStr::new("/home/alice")));
        assert_eq!(got, p("/home/alice/.local/share/moniker/transcripts"));
    }

    #[test]
    fn data_dir_without_any_base_is_relative() {
        let want = p(".local/share/moniker/transcripts");
        assert_eq!(data_dir_from(None, None), want);
        assert_eq!(
            data_dir_from(Some(OsStr::new("")), Some(OsStr::new(""))),
            want
        );
    }

    #[test]
    fn default_dir_ends_with_moniker_transcripts() {
        assert!(default_dir().ends_with("moniker/transcripts"));
    }

    /// 2026-10-17 18:57:05 UTC.
    const LATER: i64 = 1_792_263_425;

    #[test]
    fn parse_round_trips_with_transcript_path() {
        let dir = Path::new("/d");
        let secs = 1_792_266_225;
        let path = transcript_path(dir, "web-01.lan", secs);
        let want = Some(("web-01.lan".to_owned(), secs));
        assert_eq!(parse_file_name(&file_name(&path)), want);
        // The name `write_transcript` falls back to when the first is taken.
        for n in [2, 3, 10, 999] {
            assert_eq!(
                parse_file_name(&file_name(&numbered(&path, n))),
                want,
                "{n}"
            );
        }
        // Hosts that cleaning changed come back as they were written.
        for (host, kept) in [
            ("høst", "h_st"),
            ("", "unknown-host"),
            (".hidden", "_hidden"),
            ("a b/c", "a_b_c"),
        ] {
            let name = file_name(&transcript_path(dir, host, 0));
            assert_eq!(parse_file_name(&name), Some((kept.to_owned(), 0)), "{host}");
        }
        assert_eq!(
            parse_file_name(&file_name(&transcript_path(dir, "h", 0))),
            Some(("h".to_owned(), 0))
        );
    }

    #[test]
    fn parse_rejects_other_names() {
        for name in [
            "notes.txt",
            "a_b.txt",
            "x_20261317-000000.txt", // month 13
            "x_20260230-000000.txt", // 30 February
            "x_20261017-240000.txt", // hour 24
            "x_20261017-006000.txt", // minute 60
            "x_20261017-000060.txt", // second 60
            "_20261017-185705.txt",  // no host
            "h_20261017-185705",     // no extension
            "h_20261017-185705.TXT",
            "h_20261017-185705.txt.bak",
            "h_20261017-185705-.txt",
            "h_20261017-185705-x.txt",
            "h_20261017-185705-+2.txt",
            "h_20261017-185705-2-3.txt",
            "h_20261017-185705-99999999999.txt",
            "h_2026101-185705.txt",
            "h_20261017-18570.txt",
            "h_+0261017-185705.txt", // a sign is not a digit
            "h_20261017_185705.txt",
            "h_20261017185705.txt",
            "h_٢٠٢٦١٠١٧-185705.txt", // non-ASCII digits
            "",
            ".txt",
        ] {
            assert_eq!(parse_file_name(name), None, "{name:?}");
        }
    }

    #[test]
    fn a_host_with_underscores_keeps_them() {
        assert_eq!(
            parse_file_name("my_host_20261017-185705.txt"),
            Some(("my_host".to_owned(), LATER))
        );
        assert_eq!(
            parse_file_name("a__b_20261017-185705-2.txt"),
            Some(("a__b".to_owned(), LATER))
        );
        // A host that looks like a stamp is still just a host.
        assert_eq!(
            parse_file_name("h_20261017-185705_20261017-185705.txt"),
            Some(("h_20261017-185705".to_owned(), LATER))
        );
    }

    /// Names with the files they sit next to, as `list_recordings` returns them.
    fn listed(dir: &Path) -> Vec<(String, String, i64, u64)> {
        list_recordings(dir)
            .unwrap()
            .into_iter()
            .map(|r| {
                assert_eq!(r.path.parent(), Some(dir), "only files directly in dir");
                let name = r.path.file_name().unwrap().to_string_lossy();
                (name.into_owned(), r.host, r.started, r.bytes)
            })
            .collect()
    }

    #[test]
    fn listing_reads_the_host_the_time_and_the_size_from_each_file() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("b_20261017-185705.txt"), "hello").unwrap();
        fs::write(tmp.path().join("a_20261016-074500-2.txt"), "").unwrap();
        assert_eq!(
            listed(tmp.path()),
            [
                (
                    "a_20261016-074500-2.txt".to_owned(),
                    "a".to_owned(),
                    WHEN,
                    0
                ),
                ("b_20261017-185705.txt".to_owned(), "b".to_owned(), LATER, 5),
            ],
            "in file-name order"
        );
    }

    #[test]
    fn listing_skips_links_and_non_files() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("transcripts");
        fs::create_dir(&dir).unwrap();
        let outside = tmp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("passwd"), "root:x:0:0\n").unwrap();

        fs::write(dir.join("ok_20261017-185705.txt"), "x").unwrap();
        // A link to a file elsewhere, with a name that parses and one that does not.
        symlink(outside.join("passwd"), dir.join("evil_20261017-185705.txt")).unwrap();
        symlink(outside.join("passwd"), dir.join("passwd")).unwrap();
        // A link to a directory, a dangling link, a real subdirectory (with a
        // file in it) and a FIFO.
        symlink(&outside, dir.join("dirlink")).unwrap();
        symlink(outside.join("missing"), dir.join("dangling")).unwrap();
        fs::create_dir(dir.join("sub")).unwrap();
        fs::write(dir.join("sub").join("deep_20261017-185705.txt"), "x").unwrap();
        rustix::fs::mknodat(
            rustix::fs::CWD,
            dir.join("fifo_20261017-185705.txt"),
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::from_raw_mode(0o600),
            0,
        )
        .unwrap();

        let names: Vec<String> = listed(&dir).into_iter().map(|(n, ..)| n).collect();
        assert_eq!(names, ["ok_20261017-185705.txt"]);
    }

    #[test]
    fn only_transcripts_are_listed() {
        use std::os::unix::ffi::OsStrExt;
        // A folder the person also uses for other things (their home folder,
        // Downloads): only the files named the way Moniker names transcripts
        // are listed, and nothing else is read.
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        fs::write(dir.join("web_20261017-185705.txt"), "one").unwrap();
        fs::write(dir.join("web_20261016-074500-2.txt"), "two").unwrap();
        for name in [
            ".bashrc",
            "notes.txt",
            "README",
            ".txt",
            "host_badstamp.txt",
            "x_20261317-000000.txt",
            "web_20261017-185705.txt.part",
            "_20261017-185705.txt",
        ] {
            fs::write(dir.join(name), "not a transcript").unwrap();
        }
        // Not valid UTF-8, and otherwise a transcript name: not one Moniker gave.
        fs::write(
            dir.join(OsStr::from_bytes(b"caf\xe9_20261017-185705.txt")),
            "x",
        )
        .unwrap();

        assert_eq!(
            listed(dir),
            [
                (
                    "web_20261016-074500-2.txt".to_owned(),
                    "web".to_owned(),
                    WHEN,
                    3
                ),
                (
                    "web_20261017-185705.txt".to_owned(),
                    "web".to_owned(),
                    LATER,
                    3
                ),
            ]
        );
    }

    #[test]
    fn a_missing_dir_is_empty() {
        let tmp = TempDir::new().unwrap();
        assert!(
            list_recordings(&tmp.path().join("nope"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn an_empty_dir_is_empty() {
        let tmp = TempDir::new().unwrap();
        assert!(list_recordings(tmp.path()).unwrap().is_empty());
    }

    #[test]
    fn a_file_where_the_dir_should_be_is_an_error() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("f");
        fs::write(&file, "x").unwrap();
        assert!(list_recordings(&file).is_err());
    }

    // ---- Moving transcripts to another folder ----

    /// A temporary directory with a folder `from` holding the given files
    /// (written the way a session writes them, mode 0600) and the path `to` of
    /// a folder beside it that does not exist yet: the same file system.
    fn folders(files: &[(&str, &str)]) -> (TempDir, PathBuf, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let from = tmp.path().join("from");
        fs::create_dir(&from).unwrap();
        for (name, text) in files {
            write_transcript(&from.join(name), text).unwrap();
        }
        let to = tmp.path().join("to");
        (tmp, from, to)
    }

    /// Every name in `dir`, sorted.
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn make_fifo(path: &Path) {
        rustix::fs::mknodat(
            rustix::fs::CWD,
            path,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::from_raw_mode(0o600),
            0,
        )
        .unwrap();
    }

    /// The superuser ignores permission bits, so the tests that need a write
    /// to fail have nothing to test.
    fn is_superuser() -> bool {
        rustix::process::geteuid().is_root()
    }

    #[test]
    fn move_renames_within_one_file_system() {
        use std::os::unix::fs::MetadataExt;
        let (_tmp, from, to) = folders(&[
            ("a_20261016-074500.txt", "one"),
            ("b_20261017-185705.txt", "two"),
        ]);
        let inode = fs::metadata(from.join("a_20261016-074500.txt"))
            .unwrap()
            .ino();

        let report = move_transcripts(&from, &to).unwrap();

        assert_eq!(
            report,
            MoveReport {
                moved: 2,
                failed: 0,
                other: 0
            }
        );
        assert!(names(&from).is_empty(), "gone from the old folder");
        assert_eq!(
            names(&to),
            ["a_20261016-074500.txt", "b_20261017-185705.txt"]
        );
        let moved = to.join("a_20261016-074500.txt");
        assert_eq!(fs::read_to_string(&moved).unwrap(), "one");
        assert_eq!(
            fs::read_to_string(to.join("b_20261017-185705.txt")).unwrap(),
            "two"
        );
        assert_eq!(mode(&moved), 0o600);
        assert_eq!(
            fs::metadata(&moved).unwrap().ino(),
            inode,
            "renamed in place"
        );
        assert_eq!(mode(&to), 0o700, "a folder it creates is private");
    }

    #[test]
    fn move_numbers_a_taken_name() {
        const X: &str = "x_20261017-185705.txt";
        const X2: &str = "x_20261017-185705-2.txt";
        const X3: &str = "x_20261017-185705-3.txt";
        const Y: &str = "y_20261017-185705.txt";
        const Y2: &str = "y_20261017-185705-2.txt";
        let (_tmp, from, to) = folders(&[(X, "incoming"), (Y, "other")]);
        fs::create_dir(&to).unwrap();
        fs::write(to.join(X), "existing").unwrap();
        // A dangling link is a taken name too, and is not written through.
        let victim = to.join("victim");
        symlink(&victim, to.join(Y)).unwrap();

        let report = move_transcripts(&from, &to).unwrap();

        assert_eq!(
            report,
            MoveReport {
                moved: 2,
                failed: 0,
                other: 0
            }
        );
        assert_eq!(fs::read_to_string(to.join(X)).unwrap(), "existing");
        assert_eq!(fs::read_to_string(to.join(X2)).unwrap(), "incoming");
        assert_eq!(fs::read_to_string(to.join(Y2)).unwrap(), "other");
        assert!(!victim.exists(), "the link was not followed");
        assert!(names(&from).is_empty());

        // Another file with the same name takes the next number.
        write_transcript(&from.join(X), "third").unwrap();
        move_transcripts(&from, &to).unwrap();
        assert_eq!(fs::read_to_string(to.join(X3)).unwrap(), "third");
        assert_eq!(fs::read_to_string(to.join(X)).unwrap(), "existing");
        assert_eq!(fs::read_to_string(to.join(X2)).unwrap(), "incoming");
    }

    #[test]
    fn move_leaves_links_and_odd_files() {
        let (tmp, from, to) = folders(&[("ok_20261017-185705.txt", "moves")]);
        let outside = tmp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("target.txt"), "not mine").unwrap();
        symlink(
            outside.join("target.txt"),
            from.join("link_20261017-185705.txt"),
        )
        .unwrap();
        symlink(&outside, from.join("dirlink")).unwrap();
        symlink(outside.join("missing"), from.join("dangling")).unwrap();
        fs::create_dir(from.join("sub")).unwrap();
        fs::write(from.join("sub").join("deep.txt"), "deep").unwrap();
        make_fifo(&from.join("fifo_20261017-185705.txt"));

        let report = move_transcripts(&from, &to).unwrap();

        assert_eq!(
            report,
            MoveReport {
                moved: 1,
                failed: 0,
                other: 0
            }
        );
        assert_eq!(names(&to), ["ok_20261017-185705.txt"]);
        assert_eq!(
            names(&from),
            [
                "dangling",
                "dirlink",
                "fifo_20261017-185705.txt",
                "link_20261017-185705.txt",
                "sub"
            ]
        );
        for link in ["link_20261017-185705.txt", "dirlink", "dangling"] {
            let meta = fs::symlink_metadata(from.join(link)).unwrap();
            assert!(meta.file_type().is_symlink(), "{link} is still a link");
        }
        assert_eq!(
            fs::read_to_string(outside.join("target.txt")).unwrap(),
            "not mine"
        );
        assert_eq!(
            fs::read_to_string(from.join("sub").join("deep.txt")).unwrap(),
            "deep"
        );
        assert_eq!(names(&outside), ["target.txt"]);
    }

    #[test]
    fn move_carries_only_transcripts() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        // The folder was once the person's home folder or Documents: next to the
        // transcripts live files that are theirs and must not travel.
        let (_tmp, from, to) = folders(&[
            ("web_20261016-074500.txt", "one"),
            ("db_20261017-185705-2.txt", "two"),
        ]);
        let others: [(&[u8], &[u8]); 4] = [
            (b".bashrc", b"export PS1='$ '\n"),
            (b"notes.txt", b"call back \xff\x00 on monday"),
            (b"host_badstamp.txt", b"not a stamp"),
            // Not valid UTF-8, so not a name Moniker gave, even shaped like one.
            (b"h\xff_20261017-185705.txt", b"odd name"),
        ];
        for (name, text) in others {
            fs::write(from.join(OsStr::from_bytes(name)), text).unwrap();
        }
        let modified = |name: &[u8]| {
            fs::metadata(from.join(OsStr::from_bytes(name)))
                .unwrap()
                .modified()
                .unwrap()
        };
        let times: Vec<_> = others.iter().map(|(name, _)| modified(name)).collect();

        let report = move_transcripts(&from, &to).unwrap();

        assert_eq!(
            report,
            MoveReport {
                moved: 2,
                failed: 0,
                other: 4
            }
        );
        assert_eq!(
            names(&to),
            ["db_20261017-185705-2.txt", "web_20261016-074500.txt"],
            "only the transcripts arrived"
        );
        for (i, (name, text)) in others.iter().enumerate() {
            let path = from.join(OsStr::from_bytes(name));
            assert_eq!(fs::read(&path).unwrap(), *text, "{path:?} is untouched");
            assert_eq!(modified(name), times[i], "{path:?} keeps its time");
        }
        assert_eq!(names(&from).len(), 4, "nothing else left or lost");

        // Again: nothing more to move, the same others are still counted.
        assert_eq!(
            move_transcripts(&from, &to).unwrap(),
            MoveReport {
                moved: 0,
                failed: 0,
                other: 4
            }
        );
        assert_eq!(names(&to).len(), 2);
    }

    #[test]
    fn move_into_a_subfolder_of_the_old_one() {
        let (_tmp, from, _) = folders(&[
            ("a_20261016-074500.txt", "one"),
            ("b_20261017-185705.txt", "two"),
        ]);
        let to = from.join("sub");

        let report = move_transcripts(&from, &to).unwrap();

        assert_eq!(
            report,
            MoveReport {
                moved: 2,
                failed: 0,
                other: 0
            }
        );
        assert_eq!(names(&from), ["sub"], "the new folder itself stays");
        assert_eq!(
            names(&to),
            ["a_20261016-074500.txt", "b_20261017-185705.txt"]
        );
        assert_eq!(mode(&to), 0o700);
        // Nothing left to move: the folder is not a transcript.
        assert_eq!(move_transcripts(&from, &to).unwrap(), MoveReport::default());
        assert_eq!(names(&from), ["sub"]);
    }

    #[test]
    fn moving_to_the_same_folder_does_nothing() {
        use std::os::unix::fs::MetadataExt;
        let (tmp, from, _) = folders(&[("a_20261016-074500.txt", "one")]);
        let file = from.join("a_20261016-074500.txt");
        let inode = fs::metadata(&file).unwrap().ino();
        let link = tmp.path().join("link-to-from");
        symlink(&from, &link).unwrap();
        let dotted = from.join(".");
        let roundabout = from.join("..").join("from");

        for (a, b) in [
            (&from, &from),
            (&from, &link),
            (&link, &from),
            (&from, &dotted),
            (&roundabout, &from),
        ] {
            assert_eq!(
                move_transcripts(a, b).unwrap(),
                MoveReport::default(),
                "{a:?} -> {b:?}"
            );
        }
        assert_eq!(names(&from), ["a_20261016-074500.txt"]);
        assert_eq!(fs::metadata(&file).unwrap().ino(), inode);
        assert_eq!(fs::read_to_string(&file).unwrap(), "one");
    }

    #[test]
    fn a_relative_target_is_refused() {
        let (_tmp, from, _) = folders(&[("a_20261016-074500.txt", "one")]);
        let relative = Path::new("moniker-test-relative-target/x");
        let err = move_transcripts(&from, relative).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(
            !relative.exists(),
            "nothing created in the working directory"
        );
        assert_eq!(names(&from), ["a_20261016-074500.txt"]);
        assert_eq!(
            move_transcripts(&from, Path::new("")).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn preparing_a_folder_makes_it_private_and_refuses_what_cannot_take_transcripts() {
        let tmp = TempDir::new().unwrap();
        let to = tmp.path().join("new").join("folder");
        prepare_folder(&to).unwrap();
        assert_eq!(mode(&to), 0o700);
        assert_eq!(mode(&tmp.path().join("new")), 0o700, "parents too");
        prepare_folder(&to).unwrap();

        let relative = Path::new("moniker-test-relative-prepare/x");
        assert_eq!(
            prepare_folder(relative).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(
            !relative.exists(),
            "nothing created in the working directory"
        );

        let file = tmp.path().join("file");
        fs::write(&file, "x").unwrap();
        assert!(prepare_folder(&file).is_err());
        assert!(prepare_folder(&file.join("inside")).is_err());

        if !is_superuser() {
            let locked = tmp.path().join("locked");
            fs::create_dir(&locked).unwrap();
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
            let result = prepare_folder(&locked);
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        }
    }

    #[test]
    fn a_missing_source_moves_nothing_and_still_makes_the_target() {
        let tmp = TempDir::new().unwrap();
        let to = tmp.path().join("deep").join("er").join("to");
        let report = move_transcripts(&tmp.path().join("nope"), &to).unwrap();
        assert_eq!(report, MoveReport::default());
        assert_eq!(mode(&to), 0o700);
        assert_eq!(mode(&tmp.path().join("deep")), 0o700, "parents too");
    }

    #[test]
    fn a_target_that_is_a_file_is_an_error() {
        let (tmp, from, _) = folders(&[("a_20261016-074500.txt", "one")]);
        let file = tmp.path().join("file");
        fs::write(&file, "x").unwrap();
        assert!(move_transcripts(&from, &file).is_err());
        assert!(move_transcripts(&from, &file.join("inside")).is_err());
        assert_eq!(names(&from), ["a_20261016-074500.txt"]);
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "x",
            "the file is untouched"
        );
    }

    #[test]
    fn an_unwritable_target_fails_before_moving_anything() {
        if is_superuser() {
            return;
        }
        let (_tmp, from, to) = folders(&[
            ("a_20261016-074500.txt", "one"),
            ("b_20261016-074500.txt", "two"),
        ]);
        fs::create_dir(&to).unwrap();
        fs::set_permissions(&to, fs::Permissions::from_mode(0o500)).unwrap();

        let err = move_transcripts(&from, &to).unwrap_err();

        fs::set_permissions(&to, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            names(&from),
            ["a_20261016-074500.txt", "b_20261016-074500.txt"]
        );
        assert!(names(&to).is_empty());
    }

    #[test]
    fn files_that_cannot_be_moved_are_counted_and_stay() {
        const A: &str = "a_20261017-185705.txt";
        const B: &str = "b_20261017-185705.txt";
        if is_superuser() {
            return;
        }
        // Renaming out of a folder needs write permission on it.
        let (_tmp, from, to) = folders(&[(A, "one"), (B, "two")]);
        fs::set_permissions(&from, fs::Permissions::from_mode(0o500)).unwrap();

        let report = move_transcripts(&from, &to).unwrap();

        fs::set_permissions(&from, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            report,
            MoveReport {
                moved: 0,
                failed: 2,
                other: 0
            }
        );
        assert_eq!(names(&from), [A, B]);
        assert_eq!(fs::read_to_string(from.join(A)).unwrap(), "one");
        assert!(names(&to).is_empty(), "nothing half-moved");
    }

    #[test]
    fn the_copy_path_removes_the_source_only_after_a_full_copy() {
        let (_tmp, from, to) = folders(&[]);
        fs::create_dir(&to).unwrap();
        // Made with the default mode (not 0600) and big enough to need
        // several reads.
        let text: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let src = from.join("big_20261017-185705.txt");
        fs::write(&src, &text).unwrap();

        let moved = move_one(&src, &to, false).unwrap();

        assert_eq!(moved, to.join("big_20261017-185705.txt"));
        assert_eq!(fs::read(&moved).unwrap(), text, "the same bytes");
        assert_eq!(mode(&moved), 0o600, "private whatever the source was");
        assert!(!src.exists(), "the source is removed after the copy");

        // A taken name is numbered, never replaced, on this path as well.
        fs::write(&src, "second").unwrap();
        let again = move_one(&src, &to, false).unwrap();
        assert_eq!(again, to.join("big_20261017-185705-2.txt"));
        assert_eq!(fs::read_to_string(&again).unwrap(), "second");
        assert_eq!(fs::read(&moved).unwrap(), text);
        assert!(!src.exists());
    }

    #[test]
    fn a_failed_copy_leaves_no_partial_file() {
        let (tmp, from, to) = folders(&[("keep.txt", "precious")]);
        fs::create_dir(&to).unwrap();

        // A FIFO is not a regular file: refused without waiting for a writer.
        let fifo = from.join("fifo.txt");
        make_fifo(&fifo);
        assert!(move_one(&fifo, &to, false).is_err());
        assert!(fifo.exists());

        // A link is not followed, even to a regular file.
        let outside = tmp.path().join("outside.txt");
        fs::write(&outside, "not mine").unwrap();
        let link = from.join("link.txt");
        symlink(&outside, &link).unwrap();
        assert!(move_one(&link, &to, false).is_err());
        assert!(move_one(&link, &to, true).is_err(), "nor is it renamed");
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&outside).unwrap(), "not mine");

        // A directory and a missing file.
        fs::create_dir(from.join("dir.txt")).unwrap();
        assert!(move_one(&from.join("dir.txt"), &to, false).is_err());
        assert!(move_one(&from.join("missing.txt"), &to, false).is_err());

        assert!(names(&to).is_empty(), "no file left in the new folder");

        if is_superuser() {
            return;
        }
        let keep = from.join("keep.txt");

        // The new folder cannot be written to.
        fs::set_permissions(&to, fs::Permissions::from_mode(0o500)).unwrap();
        let err = move_one(&keep, &to, false);
        fs::set_permissions(&to, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(err.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(fs::read_to_string(&keep).unwrap(), "precious");
        assert!(names(&to).is_empty());

        // The copy works but the source cannot be removed (the old folder is
        // read-only): the copy is taken back, so there is one file, not two.
        fs::set_permissions(&from, fs::Permissions::from_mode(0o500)).unwrap();
        let err = move_one(&keep, &to, false);
        fs::set_permissions(&from, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(err.is_err());
        assert_eq!(fs::read_to_string(&keep).unwrap(), "precious");
        assert!(names(&to).is_empty(), "no copy left behind");
    }

    #[test]
    fn a_move_gives_up_after_many_name_clashes() {
        let (_tmp, from, to) = folders(&[("t.txt", "new")]);
        fs::create_dir(&to).unwrap();
        fs::write(to.join("t.txt"), "x").unwrap();
        for n in 2..=1000 {
            fs::write(to.join(format!("t-{n}.txt")), "x").unwrap();
        }
        for rename_first in [true, false] {
            let err = move_one(&from.join("t.txt"), &to, rename_first).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{rename_first}");
            assert_eq!(fs::read_to_string(from.join("t.txt")).unwrap(), "new");
            assert_eq!(names(&to).len(), 1000);
        }
    }

    #[test]
    fn an_unwritable_target_is_refused_even_with_nothing_to_move() {
        if is_superuser() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let to = tmp.path().join("to");
        fs::create_dir(&to).unwrap();
        fs::set_permissions(&to, fs::Permissions::from_mode(0o500)).unwrap();
        let empty = tmp.path().join("empty");
        fs::create_dir(&empty).unwrap();

        // No old folder at all, an empty one, and the very same folder.
        let results = [
            move_transcripts(&tmp.path().join("nope"), &to),
            move_transcripts(&empty, &to),
            move_transcripts(&to, &to),
        ];

        fs::set_permissions(&to, fs::Permissions::from_mode(0o700)).unwrap();
        for result in results {
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        }
        assert!(names(&to).is_empty());
    }

    #[test]
    fn the_copy_keeps_the_modification_time() {
        let (_tmp, from, to) = folders(&[("a_20261016-074500.txt", "abc")]);
        fs::create_dir(&to).unwrap();
        let mtime = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let src = from.join("a_20261016-074500.txt");
        fs::File::options()
            .write(true)
            .open(&src)
            .unwrap()
            .set_modified(mtime)
            .unwrap();

        let moved = move_one(&src, &to, false).unwrap();

        assert_eq!(
            fs::metadata(&moved).unwrap().modified().unwrap(),
            mtime,
            "when the session ended, not when it moved"
        );
    }

    #[test]
    fn a_folder_can_be_synced_and_nothing_else() {
        let tmp = TempDir::new().unwrap();
        sync_dir(tmp.path()).unwrap();
        let file = tmp.path().join("f");
        fs::write(&file, "x").unwrap();
        assert!(sync_dir(&file).is_err());
        assert!(sync_dir(&tmp.path().join("nope")).is_err());
    }

    #[test]
    fn a_file_removed_after_it_was_listed_is_neither_moved_nor_failed() {
        let (_tmp, from, to) = folders(&[("a.txt", "one"), ("b.txt", "two")]);
        fs::create_dir(&to).unwrap();
        let files = [
            from.join("a.txt"),
            from.join("gone.txt"), // listed, then deleted by someone else
            from.join("b.txt"),
        ];

        let report = move_files(&files, &to);

        assert_eq!(
            report,
            MoveReport {
                moved: 2,
                failed: 0,
                other: 0
            }
        );
        assert_eq!(names(&to), ["a.txt", "b.txt"]);
    }

    #[test]
    fn a_vanished_target_is_a_failure_for_a_file_that_is_still_there() {
        let (_tmp, from, to) = folders(&[("a.txt", "one")]);
        // `to` was never made (or was removed): the file is still whole.
        let report = move_files(&[from.join("a.txt")], &to);
        assert_eq!(
            report,
            MoveReport {
                moved: 0,
                failed: 1,
                other: 0
            }
        );
        assert_eq!(fs::read_to_string(from.join("a.txt")).unwrap(), "one");
    }

    #[test]
    fn a_file_removed_during_the_copy_leaves_nothing_behind() {
        let (_tmp, from, to) = folders(&[("a.txt", "one")]);
        fs::create_dir(&to).unwrap();
        after_the_copy(|src| fs::remove_file(src).unwrap());

        let err = move_one(&from.join("a.txt"), &to, false).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(names(&to).is_empty(), "no copy of a file that was deleted");
        // ...so the move counts it as neither moved nor failed.
        assert!(is_gone(&from.join("a.txt")));
    }

    #[test]
    fn a_file_replaced_during_the_copy_is_not_removed() {
        let (tmp, from, to) = folders(&[("a.txt", "the original")]);
        fs::create_dir(&to).unwrap();
        let replacement = tmp.path().join("replacement");
        fs::write(&replacement, "a different file").unwrap();
        after_the_copy(move |src| fs::rename(&replacement, src).unwrap());

        move_one(&from.join("a.txt"), &to, false).unwrap_err();

        assert_eq!(
            fs::read_to_string(from.join("a.txt")).unwrap(),
            "a different file",
            "the new file under the old name is left alone"
        );
        assert!(
            names(&to).is_empty(),
            "the copy of the replaced file is gone"
        );
    }

    #[test]
    fn a_file_written_to_during_the_copy_stays() {
        let (_tmp, from, to) = folders(&[("grows.txt", "start"), ("touched.txt", "same")]);
        fs::create_dir(&to).unwrap();

        // Appended to (a session still writing): the copy would be short.
        after_the_copy(|src| {
            let mut file = fs::OpenOptions::new().append(true).open(src).unwrap();
            file.write_all(b" and more").unwrap();
        });
        move_one(&from.join("grows.txt"), &to, false).unwrap_err();
        assert_eq!(
            fs::read_to_string(from.join("grows.txt")).unwrap(),
            "start and more"
        );

        // Same size, different time.
        after_the_copy(|src| {
            let later =
                fs::metadata(src).unwrap().modified().unwrap() + std::time::Duration::from_secs(60);
            fs::File::options()
                .write(true)
                .open(src)
                .unwrap()
                .set_modified(later)
                .unwrap();
        });
        move_one(&from.join("touched.txt"), &to, false).unwrap_err();
        assert_eq!(
            fs::read_to_string(from.join("touched.txt")).unwrap(),
            "same"
        );

        assert!(names(&to).is_empty(), "no copy left in the new folder");

        // Nothing changed: the same call moves it.
        let moved = move_one(&from.join("touched.txt"), &to, false).unwrap();
        assert_eq!(fs::read_to_string(moved).unwrap(), "same");
        assert!(!from.join("touched.txt").exists());
    }
}
