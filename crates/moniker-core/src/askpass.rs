//! Decision logic of the `moniker-askpass` helper.
//!
//! With `SSH_ASKPASS_REQUIRE=force`, OpenSSH runs the helper once per prompt,
//! with the prompt text as its argument. That covers password prompts, key
//! passphrase prompts and the host-key question "Are you sure you want to
//! continue connecting?". [`answer`] decides what to do with one prompt:
//!
//! 1. `SSH_ASKPASS_PROMPT=none` marks a message to display, not a question:
//!    show it on the terminal and answer with nothing.
//! 2. `SSH_ASKPASS_PROMPT=confirm` marks a yes/no question, which is host-key
//!    confirmation. It always goes to the user; the helper never answers it.
//! 3. A password prompt for this profile's own login is answered from the
//!    keyring for the profile named by `MONIKER_PROFILE_ID`. That id arrives
//!    through the environment, so it is only used if it is a canonical UUID and
//!    so cannot address any other keyring entry. "For this profile's own login"
//!    is decided twice:
//!    * By who asks: only the `ssh` that Moniker started. The helper's caller
//!      checks that the parent of the `ssh` running the helper is the Moniker
//!      process named by [`PID_ENV`] ([`started_by_moniker`]) and passes no
//!      identity otherwise. A `ProxyJump` or `ProxyCommand` hop runs its own
//!      `ssh` as a child of the first one, so a hop's prompt never qualifies,
//!      whatever host it names.
//!    * By the prompt's identity: `ssh` puts the user and host it is
//!      authenticating to in the prompt, so the caller passes the identity
//!      this connection logs in as and the prompt must be exactly
//!      `<user>@<host>'s password:` (password authentication) or
//!      `(<user>@<host>) Password:` (keyboard-interactive; the word is matched
//!      without regard to case). The identity is what `ssh -G` reports for the
//!      profile's connection ([`effective_identity`]: after `~/.ssh/config`,
//!      so an alias, a `HostName` or a `HostKeyAlias` is followed), or the
//!      profile's own `(username, host)` if that fails or is ambiguous. It is
//!      compared literally and case sensitively.
//!
//!    Anything else, in particular a hop's prompt, another host's, a bare
//!    `Password:` or `New password:`, never receives the stored password.
//! 4. Anything else is put to the user.
//!
//! Rule 3 has a second condition in a terminal tab: the stored password is
//! sent at most once per connection. A wrong one makes the server ask again,
//! and that prompt (and every later one) must go to the user, never get the
//! same password again. `ssh` runs a fresh helper for each prompt, so the
//! helpers of one connection share a random one-time token
//! ([`AskpassToken`], in [`TOKEN_ENV`]; not a secret) and a marker file for it
//! ([`claim_marker`]: `$XDG_RUNTIME_DIR/moniker/askpass/<token>`, created with
//! `O_CREAT|O_EXCL`). A prompt that qualifies for the stored password is
//! answered from the keyring only by the helper that creates the marker; if it
//! already exists, or cannot be created (no usable runtime directory, a
//! directory on the way that is a symlink, someone else's, or open to others),
//! the prompt goes to the user ([`OneTime`]). Failing to make the marker means
//! asking the user: it fails closed. Prompts that never qualified (host-key
//! questions, notices, passphrases, other hosts) do not touch the marker, so a
//! first connection still logs in by itself after the host-key question. The
//! GUI removes the marker when the connection ends ([`release_marker`]). A sync
//! has no terminal to fall back to: it passes no token ([`OneTime::NotUsed`])
//! and `ssh` itself allows it one password prompt.
//!
//! Known limits of rule 3: when `ssh -G` cannot say (it fails, takes too long
//! or reports something ambiguous), or the user or host is longer than `ssh`
//! prints in full, the identity does not match and the user types the password
//! at the terminal; so does an `ssh` on `PATH` that is a wrapper starting the
//! real one as its child. That is a safe degradation, not a failure.
//!
//! "The user" means `/dev/tty` ([`DevTty`]). It exists when `ssh` runs inside
//! the terminal widget. A headless `ssh` (sync) has no controlling terminal, so
//! every [`DevTty`] call fails, the helper fails, and `ssh` fails closed rather
//! than hanging or accepting anything unseen.
//!
//! Neither the prompt text nor any answer is ever logged. Answers are returned
//! in [`Zeroizing`] strings. Echo is switched off with termios (through the
//! `rustix` crate) the way OpenSSH's own `readpassphrase` does it: clear
//! `ECHO`, keep the line discipline, restore on the way out.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, CWD, FileType, Mode, OFlags, fchmod, fstat, mkdirat, openat, unlinkat};
use rustix::io::Errno;
use rustix::process::geteuid;
use rustix::termios::{LocalModes, OptionalActions, Termios, tcgetattr, tcsetattr};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::profile::is_canonical_uuid;
use crate::secrets::{SecretStore, password_key};

/// The controlling terminal of the process.
const TTY_PATH: &str = "/dev/tty";

/// The environment variable that carries a connection's [`AskpassToken`] to
/// the helper.
pub const TOKEN_ENV: &str = "MONIKER_ASKPASS_TOKEN";

/// The environment variable that carries the process id of the Moniker
/// process that starts `ssh` (the GUI, or `moniker-server` for a sync from the
/// command line). The helper answers only for an `ssh` whose parent that is
/// ([`started_by_moniker`]). Not a secret.
pub const PID_ENV: &str = "MONIKER_PID";

/// The directory of Moniker's files in the runtime directory, and the one of
/// the markers in it: `<runtime>/moniker/askpass`.
const RUNTIME_SUBDIR: &str = "moniker";
const MARKER_SUBDIR: &str = "askpass";

/// The one-time token of one terminal connection: a random v4 UUID, lowercase
/// and hyphenated, which names its marker (module docs). Not a secret: it only
/// tells the helpers of one connection apart from those of another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskpassToken(String);

impl AskpassToken {
    /// A fresh random token.
    pub fn new() -> AskpassToken {
        AskpassToken(Uuid::new_v4().hyphenated().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for AskpassToken {
    /// A fresh random token, as [`AskpassToken::new`].
    fn default() -> Self {
        AskpassToken::new()
    }
}

/// Where the markers live: `<runtime_dir>/moniker/askpass`.
pub fn marker_dir(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join(RUNTIME_SUBDIR).join(MARKER_SUBDIR)
}

/// Create the marker for `token` in [`marker_dir`]`(runtime_dir)`: `Ok(true)`
/// if this call created it, `Ok(false)` if it already existed (the stored
/// password was already used for this connection).
///
/// An error, and nothing created, if `runtime_dir` is not absolute or `token`
/// is not a canonical UUID (so a token cannot name any other path). The two
/// directories are created with mode 0700 if missing; an existing one must be
/// a real directory (not a symlink), owned by this user, with no group or other
/// permission bits, or this is an error. No symlink below `runtime_dir` is
/// followed: each directory is opened relative to the one above it with
/// `O_NOFOLLOW`, and the marker is created there with
/// `O_CREAT|O_EXCL|O_NOFOLLOW`, mode 0600 (the umask can only remove bits). It
/// is empty: its existence is what counts.
pub fn claim_marker(runtime_dir: &Path, token: &str) -> io::Result<bool> {
    let dir = open_marker_dir(runtime_dir, token, true)?;
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    match openat(&dir, token, flags, Mode::RUSR | Mode::WUSR) {
        Ok(_marker) => Ok(true),
        Err(Errno::EXIST) => Ok(false),
        Err(err) => Err(err.into()),
    }
}

/// Remove the marker for `token`, if there is one, when its connection has
/// ended. Does nothing for a token that is not a canonical UUID, a runtime
/// directory that is not absolute, or a marker directory that [`claim_marker`]
/// would refuse; errors are ignored.
pub fn release_marker(runtime_dir: &Path, token: &str) {
    if let Ok(dir) = open_marker_dir(runtime_dir, token, false) {
        let _ = unlinkat(&dir, token, AtFlags::empty());
    }
}

/// The marker directory, opened, after checking the arguments and each
/// directory on the way (see [`claim_marker`]). With `create`, missing
/// directories are made.
fn open_marker_dir(runtime_dir: &Path, token: &str, create: bool) -> io::Result<OwnedFd> {
    if !runtime_dir.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the runtime directory is not an absolute path",
        ));
    }
    if !is_canonical_uuid(token) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the askpass token is not a canonical UUID",
        ));
    }
    let runtime = openat(
        CWD,
        runtime_dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let moniker = private_dir(&runtime, RUNTIME_SUBDIR, create)?;
    private_dir(&moniker, MARKER_SUBDIR, create)
}

/// The directory `name` in `parent`, opened without following a symlink, if it
/// is private: a directory, owned by this user, with no group or other bits.
/// With `create` it is made (mode 0700, set explicitly so the umask cannot
/// change it) if missing.
fn private_dir(parent: impl AsFd, name: &str, create: bool) -> io::Result<OwnedFd> {
    let created = create
        && match mkdirat(&parent, name, Mode::RWXU) {
            Ok(()) => true,
            // Maybe a directory, maybe a symlink or a file: opening it tells.
            Err(Errno::EXIST) => false,
            Err(err) => return Err(err.into()),
        };
    let dir = openat(
        &parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    if created {
        fchmod(&dir, Mode::RWXU)?;
    }
    let stat = fstat(&dir)?;
    let private = FileType::from_raw_mode(stat.st_mode) == FileType::Directory
        && stat.st_uid == geteuid().as_raw()
        && stat.st_mode & 0o077 == 0;
    if !private {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the askpass marker directory is not private to this user",
        ));
    }
    Ok(dir)
}

/// Whether the stored password may be sent for this prompt, as far as the
/// one-time marker goes (module docs).
pub enum OneTime<'a> {
    /// No marker: the stored password answers every prompt that qualifies.
    /// For sync, where `ssh` itself allows a single password prompt.
    NotUsed,
    /// Call this once a prompt qualifies for the stored password: `true` means
    /// this call created the connection's marker and may send it; `false`
    /// (already claimed, or no marker could be made) means the user is asked.
    Claim(&'a dyn Fn() -> bool),
}

impl OneTime<'_> {
    /// Whether the stored password may be sent now. Claims the marker.
    fn allows(&self) -> bool {
        match self {
            OneTime::NotUsed => true,
            OneTime::Claim(claim) => claim(),
        }
    }
}

/// The user, as reached by the askpass helper.
pub trait TtyPrompter {
    /// Show `prompt` and read one line, without its trailing newline. With
    /// `echo == false` the typed text is not shown.
    fn ask(&mut self, prompt: &str, echo: bool) -> io::Result<String>;
    /// Show `msg` and a newline. Reads nothing.
    fn notify(&mut self, msg: &str) -> io::Result<()>;
}

/// What to reply to one `ssh` prompt. See the module docs for the rules.
///
/// * `prompt`: the text `ssh` passed as the helper's argument.
/// * `askpass_prompt_env`: the value of `SSH_ASKPASS_PROMPT`, if set.
/// * `profile_id`: the value of `MONIKER_PROFILE_ID`, if set.
/// * `expected`: the `(user, host)` this connection logs in as, if the caller
///   could tell: what `ssh -G` reports for the profile `profile_id` names, or
///   the profile's own user and host ([`effective_identity`]); `None` when the
///   prompting `ssh` is not the one Moniker started ([`started_by_moniker`]),
///   which is what keeps a `ProxyJump` or `ProxyCommand` hop from it. The
///   stored password is only ever used for a prompt that names exactly this
///   user and host (module docs, rule 3). `None` means no prompt is answered
///   automatically.
/// * `once`: whether the stored password is limited to one use per connection
///   ([`OneTime::Claim`], terminal tabs) or not ([`OneTime::NotUsed`], sync).
///   The claim is made only for a prompt that would otherwise get the stored
///   password (the right identity, not a passphrase, a canonical id, a
///   password stored); if it fails the user is asked, with the input hidden.
///
/// A failure of `tty` is returned as an error. A failure of `secrets` is not:
/// the user is asked instead.
pub fn answer(
    prompt: &str,
    askpass_prompt_env: Option<&str>,
    profile_id: Option<&str>,
    expected: Option<(&str, &str)>,
    once: OneTime<'_>,
    secrets: &dyn SecretStore,
    tty: &mut dyn TtyPrompter,
) -> io::Result<Zeroizing<String>> {
    match askpass_prompt_env {
        Some("none") => {
            tty.notify(prompt)?;
            return Ok(Zeroizing::new(String::new()));
        }
        Some("confirm") => return tty.ask(prompt, true).map(Zeroizing::new),
        _ => {}
    }

    let lower = prompt.to_lowercase();
    if let Some((user, host)) = expected
        && is_login_password_prompt(prompt, user, host)
        && !lower.contains("passphrase")
        && let Some(id) = profile_id.filter(|id| is_canonical_uuid(id))
        && let Ok(Some(secret)) = secrets.get(&password_key(id))
        // Last, so only a prompt that qualifies uses up the marker.
        && once.allows()
    {
        return Ok(secret);
    }

    let hidden = lower.contains("passphrase")
        || lower.contains("password")
        || lower.contains("code")
        || has_word(&lower, "pin");
    tty.ask(prompt, !hidden).map(Zeroizing::new)
}

/// True if `prompt` is, apart from trailing whitespace, exactly one of the two
/// password prompts `ssh` shows for logging in as `user` on `host`:
/// `<user>@<host>'s password:` or `(<user>@<host>) Password:` (any case for the
/// word). `user` and `host` are compared literally and case sensitively; an
/// empty `user` or `host` matches nothing.
fn is_login_password_prompt(prompt: &str, user: &str, host: &str) -> bool {
    if user.is_empty() || host.is_empty() {
        return false;
    }
    let prompt = prompt.trim_end();
    if after_identity(prompt, user, host) == Some("'s password:") {
        return true;
    }
    prompt
        .strip_prefix('(')
        .and_then(|rest| after_identity(rest, user, host))
        .and_then(|rest| rest.strip_prefix(") "))
        .is_some_and(|word| word.eq_ignore_ascii_case("password:"))
}

/// What follows `<user>@<host>` at the start of `text`, if `text` starts with it.
fn after_identity<'a>(text: &'a str, user: &str, host: &str) -> Option<&'a str> {
    text.strip_prefix(user)?
        .strip_prefix('@')?
        .strip_prefix(host)
}

/// True if `word` is one of the alphanumeric words of `lower` (a lowercased
/// prompt): `pin` is in "enter pin for key" but not in "spinning".
fn has_word(lower: &str, word: &str) -> bool {
    lower
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| w == word)
}

/// The `(user, host)` that `ssh`'s password prompt names for a connection,
/// read from what `ssh -G` printed for it (`crate::ssh::resolve_command`):
/// the `user`, and the `hostkeyalias` if there is one (the prompt names it),
/// else the `hostname`, as `ssh` prints them (it lowercases both host names).
/// This is the identity after `~/.ssh/config` is applied, so a profile whose
/// host is an alias, or is rewritten by `HostName`, still matches its prompt.
///
/// `None`, and the caller keeps to the profile's own host, unless the dump is
/// unambiguous: `user` and `hostname` present, each of the three keys at most
/// once, every value one word with no whitespace or control characters, and
/// the user the profile's own `username` (`ssh` prefers the user on its
/// command line to the configuration's, so another one means something is
/// off).
pub fn effective_identity(dump: &str, username: &str) -> Option<(String, String)> {
    let (mut user, mut hostname, mut alias) = (None, None, None);
    for line in dump.lines() {
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        let slot = match key {
            "user" => &mut user,
            "hostname" => &mut hostname,
            "hostkeyalias" => &mut alias,
            _ => continue,
        };
        if slot.replace(value).is_some() {
            return None;
        }
    }
    let word = |v: &&str| !v.is_empty() && !v.chars().any(|c| c.is_whitespace() || c.is_control());
    let user = user.filter(word)?;
    let hostname = hostname.filter(word)?;
    let host = match alias {
        None => hostname,
        Some(alias) => Some(alias).filter(word)?,
    };
    (user == username).then(|| (user.to_owned(), host.to_owned()))
}

/// Whether `prompt` could be the password prompt of logging in as `user`, before
/// the host is known: it starts with `<user>@` or `(<user>@`, and, apart from
/// trailing whitespace, ends in `password:` in any case. A first look that
/// saves running `ssh -G` for a key passphrase, a code or another user's
/// prompt; [`answer`] makes the exact match. An empty `user` matches nothing.
pub fn could_be_login_prompt(prompt: &str, user: &str) -> bool {
    if user.is_empty() {
        return false;
    }
    let prompt = prompt.trim_end();
    let identity_first = prompt
        .strip_prefix('(')
        .unwrap_or(prompt)
        .strip_prefix(user)
        .is_some_and(|rest| rest.starts_with('@'));
    identity_first && prompt.to_lowercase().ends_with("password:")
}

/// The parent process id in `stat`, the text of a `/proc/<pid>/stat` file: the
/// fourth field, the second after the command name. The command name is in
/// parentheses and may hold spaces and parentheses itself, so it ends at the
/// last `)`. `None` if there is no such field or it is not a plain number.
pub fn parent_in_stat(stat: &str) -> Option<u32> {
    let (_, after_name) = stat.rsplit_once(')')?;
    let mut fields = after_name.split_ascii_whitespace();
    let _state = fields.next()?;
    plain_number(fields.next()?)
}

/// Whether the `ssh` that runs the helper was started by Moniker itself:
/// `prompting_ssh_parent` (the parent of the helper's parent, read from
/// `/proc`) is the process id Moniker put in [`PID_ENV`], passed here as
/// `launcher`. A `ProxyJump` or `ProxyCommand` hop runs its own `ssh` as a
/// child of the first one, so a hop's prompt never passes, whatever host it
/// names. Neither id may be missing, and `launcher` must be a plain number
/// above 1: an `ssh` whose parent has gone is adopted by init (or a
/// subreaper), which is never Moniker.
pub fn started_by_moniker(launcher: Option<&str>, prompting_ssh_parent: Option<u32>) -> bool {
    launcher
        .and_then(plain_number)
        .is_some_and(|launcher| launcher > 1 && prompting_ssh_parent == Some(launcher))
}

/// `text` as a number if it is only ASCII digits (no sign, no spaces).
fn plain_number(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// The real [`TtyPrompter`]: talks to `/dev/tty`, which it opens afresh for each
/// call. Without a controlling terminal every call returns an error.
pub struct DevTty;

impl TtyPrompter for DevTty {
    fn ask(&mut self, prompt: &str, echo: bool) -> io::Result<String> {
        ask_on(&open_tty()?, prompt, echo)
    }

    fn notify(&mut self, msg: &str) -> io::Result<()> {
        notify_on(&mut open_tty()?, msg)
    }
}

fn open_tty() -> io::Result<File> {
    OpenOptions::new().read(true).write(true).open(TTY_PATH)
}

/// Prompt on `tty` and read a line from it.
///
/// With `echo == false` echo is switched off first, so nothing typed is ever
/// shown, and back on afterwards (also on error). If echo cannot be switched
/// off, for instance because `tty` is not a terminal, this fails without
/// writing or reading anything: a secret is never echoed as a fallback.
fn ask_on(tty: &File, prompt: &str, echo: bool) -> io::Result<String> {
    let mut out = tty;
    let mut input = tty;
    if echo {
        out.write_all(printable(prompt).as_bytes())?;
        out.flush()?;
        return read_line(&mut input);
    }

    let silenced = EchoOff::new(tty)?;
    let line = out
        .write_all(printable(prompt).as_bytes())
        .and_then(|()| out.flush())
        .and_then(|()| read_line(&mut input));
    drop(silenced);
    // The newline the user typed was not echoed, so end the prompt's line.
    let _ = out.write_all(b"\n");
    line
}

/// Write `msg` and a newline to `out`.
fn notify_on(out: &mut impl Write, msg: &str) -> io::Result<()> {
    out.write_all(printable(msg).as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()
}

/// Echo switched off on a terminal until dropped. If the process is killed
/// meanwhile (Ctrl-C reaches `ssh` and the helper alike) echo stays off; the
/// session they belong to is ending anyway.
struct EchoOff<'a> {
    tty: &'a File,
    original: Termios,
}

impl<'a> EchoOff<'a> {
    fn new(tty: &'a File) -> io::Result<Self> {
        let original = tcgetattr(tty)?;
        let mut silent = original.clone();
        // `ECHONL` echoes the newline even with `ECHO` off; the caller ends the line.
        silent
            .local_modes
            .remove(LocalModes::ECHO | LocalModes::ECHONL);
        // Like `readpassphrase`: input typed before this point was already
        // shown, so it is discarded rather than read as part of the secret.
        tcsetattr(tty, OptionalActions::Flush, &silent)?;
        Ok(EchoOff { tty, original })
    }
}

impl Drop for EchoOff<'_> {
    fn drop(&mut self) {
        let _ = tcsetattr(self.tty, OptionalActions::Now, &self.original);
    }
}

/// Read up to and including the next newline; return the line without it.
///
/// Reads a byte at a time so nothing past the newline is consumed. Bytes are
/// held in zeroizing buffers (reserved up front so growing rarely copies).
/// End of input before any byte is an error (the user pressed Ctrl-D); end of
/// input after some bytes ends the line. Not valid UTF-8 is an error.
fn read_line(reader: &mut impl Read) -> io::Result<String> {
    let mut line = Zeroizing::new(Vec::with_capacity(256));
    let mut byte = Zeroizing::new([0u8; 1]);
    let mut ended_by_newline = false;
    loop {
        match reader.read(&mut byte[..]) {
            Ok(0) => break,
            Ok(_) if byte[0] == b'\n' => {
                ended_by_newline = true;
                break;
            }
            Ok(_) => line.push(byte[0]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    if line.is_empty() && !ended_by_newline {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "end of input while reading an answer",
        ));
    }
    String::from_utf8(std::mem::take(&mut *line)).map_err(|err| {
        drop(Zeroizing::new(err.into_bytes()));
        io::Error::new(io::ErrorKind::InvalidData, "answer is not valid UTF-8")
    })
}

/// `text` with control characters other than newline and tab replaced by `?`.
///
/// Prompt text may come from the remote server (keyboard-interactive), and
/// escape sequences in it must not act on the user's terminal.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() && !matches!(c, '\n' | '\t') {
                '?'
            } else {
                c
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::{MemorySecretStore, SecretError, password_key};
    use std::cell::Cell;

    const ID: &str = "00000000-0000-4000-8000-000000000001";
    const SECRET: &str = "s3cret-pw";
    const USER: &str = "alice";
    const HOST: &str = "web1.example.com";
    /// The (username, host) the helper looked up for the profile `ID`.
    const EXPECTED: Option<(&str, &str)> = Some((USER, HOST));
    const PASSWORD_PROMPTS: [&str; 2] = [
        "alice@web1.example.com's password: ",
        "(alice@web1.example.com) Password: ",
    ];
    const HOST_KEY_PROMPT: &str =
        "Are you sure you want to continue connecting (yes/no/[fingerprint])? ";
    const PASSPHRASE_PROMPT: &str = "Enter passphrase for key '/home/u/.ssh/id_ed25519': ";

    /// Records every call. `reply` is what the "user" types; `None` models no
    /// controlling terminal, so every call fails.
    #[derive(Default)]
    struct FakeTty {
        reply: Option<String>,
        asked: Vec<(String, bool)>,
        notified: Vec<String>,
    }

    impl FakeTty {
        fn replying(reply: &str) -> Self {
            FakeTty {
                reply: Some(reply.to_owned()),
                ..FakeTty::default()
            }
        }

        fn untouched(&self) -> bool {
            self.asked.is_empty() && self.notified.is_empty()
        }
    }

    fn no_tty() -> io::Error {
        io::Error::new(io::ErrorKind::NotFound, "no controlling terminal")
    }

    impl TtyPrompter for FakeTty {
        fn ask(&mut self, prompt: &str, echo: bool) -> io::Result<String> {
            self.asked.push((prompt.to_owned(), echo));
            self.reply.clone().ok_or_else(no_tty)
        }

        fn notify(&mut self, msg: &str) -> io::Result<()> {
            self.notified.push(msg.to_owned());
            self.reply.as_ref().map(|_| ()).ok_or_else(no_tty)
        }
    }

    /// A store holding `SECRET` for the profile `ID`.
    fn store_with_secret() -> MemorySecretStore {
        let store = MemorySecretStore::default();
        store.set(&password_key(ID), SECRET).unwrap();
        store
    }

    /// A store whose every lookup fails.
    struct BrokenStore;

    impl SecretStore for BrokenStore {
        fn get(&self, _key: &str) -> Result<Option<Zeroizing<String>>, SecretError> {
            Err(SecretError::Unavailable("no session bus".to_owned()))
        }
        fn set(&self, _key: &str, _value: &str) -> Result<(), SecretError> {
            Err(SecretError::Unavailable("no session bus".to_owned()))
        }
        fn delete(&self, _key: &str) -> Result<(), SecretError> {
            Err(SecretError::Unavailable("no session bus".to_owned()))
        }
        fn probe(&self) -> Result<(), SecretError> {
            Err(SecretError::Unavailable("no session bus".to_owned()))
        }
    }

    #[test]
    fn password_prompt_returns_stored_secret() {
        let store = store_with_secret();
        for prompt in PASSWORD_PROMPTS {
            let mut tty = FakeTty::default();
            let got = answer(
                prompt,
                None,
                Some(ID),
                EXPECTED,
                OneTime::NotUsed,
                &store,
                &mut tty,
            )
            .unwrap();
            assert_eq!(got.as_str(), SECRET, "prompt {prompt:?}");
            assert!(tty.untouched(), "tty used for {prompt:?}");
        }
    }

    #[test]
    fn host_key_confirmation_goes_to_tty() {
        // The secret exists, but a host-key question must never be answered by us.
        let store = store_with_secret();
        let mut tty = FakeTty::replying("yes");
        let got = answer(
            HOST_KEY_PROMPT,
            Some("confirm"),
            Some(ID),
            EXPECTED,
            OneTime::NotUsed,
            &store,
            &mut tty,
        )
        .unwrap();
        assert_eq!(got.as_str(), "yes", "the user's own answer, not the secret");
        assert_eq!(tty.asked, [(HOST_KEY_PROMPT.to_owned(), true)]);
        assert!(tty.notified.is_empty());
    }

    #[test]
    fn host_key_prompt_without_env_marker_is_still_not_answered() {
        let store = store_with_secret();
        let mut tty = FakeTty::replying("no");
        let got = answer(
            HOST_KEY_PROMPT,
            None,
            Some(ID),
            EXPECTED,
            OneTime::NotUsed,
            &store,
            &mut tty,
        )
        .unwrap();
        assert_eq!(got.as_str(), "no");
        assert_eq!(tty.asked, [(HOST_KEY_PROMPT.to_owned(), true)]);
    }

    #[test]
    fn confirm_marker_wins_over_a_password_looking_prompt() {
        let store = store_with_secret();
        let mut tty = FakeTty::replying("typed");
        let got = answer(
            PASSWORD_PROMPTS[0],
            Some("confirm"),
            Some(ID),
            EXPECTED,
            OneTime::NotUsed,
            &store,
            &mut tty,
        )
        .unwrap();
        assert_eq!(got.as_str(), "typed");
        assert_eq!(tty.asked.len(), 1);
        assert!(tty.asked[0].1, "confirm prompts are echoed");
    }

    #[test]
    fn key_passphrase_goes_to_tty_without_echo() {
        let store = store_with_secret();
        let mut tty = FakeTty::replying("my-passphrase");
        let got = answer(
            PASSPHRASE_PROMPT,
            None,
            Some(ID),
            EXPECTED,
            OneTime::NotUsed,
            &store,
            &mut tty,
        )
        .unwrap();
        assert_eq!(got.as_str(), "my-passphrase");
        assert_eq!(tty.asked, [(PASSPHRASE_PROMPT.to_owned(), false)]);
    }

    #[test]
    fn passphrase_prompt_ending_in_password_colon_is_not_answered() {
        let store = store_with_secret();
        let mut tty = FakeTty::replying("typed");
        let prompt = "Enter passphrase for the saved password:";
        answer(
            prompt,
            None,
            Some(ID),
            EXPECTED,
            OneTime::NotUsed,
            &store,
            &mut tty,
        )
        .unwrap();
        assert_eq!(tty.asked, [(prompt.to_owned(), false)]);
    }

    #[test]
    fn missing_secret_falls_back_to_tty() {
        let store = MemorySecretStore::default();
        for prompt in PASSWORD_PROMPTS {
            let mut tty = FakeTty::replying("typed-pw");
            let got = answer(
                prompt,
                None,
                Some(ID),
                EXPECTED,
                OneTime::NotUsed,
                &store,
                &mut tty,
            )
            .unwrap();
            assert_eq!(got.as_str(), "typed-pw");
            assert_eq!(tty.asked, [(prompt.to_owned(), false)]);
        }
    }

    #[test]
    fn secret_store_error_falls_back_to_tty() {
        let mut tty = FakeTty::replying("typed-pw");
        let prompt = PASSWORD_PROMPTS[0];
        let got = answer(
            prompt,
            None,
            Some(ID),
            EXPECTED,
            OneTime::NotUsed,
            &BrokenStore,
            &mut tty,
        )
        .unwrap();
        assert_eq!(got.as_str(), "typed-pw");
        assert_eq!(tty.asked, [(prompt.to_owned(), false)]);
    }

    #[test]
    fn non_uuid_profile_id_is_ignored() {
        // A matching key exists for each hostile id, so only the id check stops us.
        let hostile = [
            "../x",
            "",
            "not-a-uuid",
            "ABCDEF01-2345-4678-8ABC-DEF012345678", // uppercase
            "{abcdef01-2345-4678-8abc-def012345678}",
            "abcdef0123454678-8abc-def012345678", // not hyphenated
        ];
        let store = MemorySecretStore::default();
        for id in hostile {
            store.set(&password_key(id), SECRET).unwrap();
        }
        for id in hostile {
            let mut tty = FakeTty::replying("typed-pw");
            let got = answer(
                PASSWORD_PROMPTS[0],
                None,
                Some(id),
                EXPECTED,
                OneTime::NotUsed,
                &store,
                &mut tty,
            )
            .unwrap();
            assert_eq!(
                got.as_str(),
                "typed-pw",
                "id {id:?} must not reach the keyring"
            );
            assert_eq!(tty.asked.len(), 1, "id {id:?}");
        }
    }

    #[test]
    fn missing_profile_id_goes_to_tty() {
        let store = store_with_secret();
        let mut tty = FakeTty::replying("typed-pw");
        let got = answer(
            PASSWORD_PROMPTS[0],
            None,
            None,
            EXPECTED,
            OneTime::NotUsed,
            &store,
            &mut tty,
        )
        .unwrap();
        assert_eq!(got.as_str(), "typed-pw");
        assert_eq!(tty.asked.len(), 1);
    }

    /// `answer` for a password-looking `prompt` with the store holding the
    /// secret and the user typing "typed-pw" if asked. Returns what `answer`
    /// returned and whether the user had to be asked.
    fn answered_for(prompt: &str, expected: Option<(&str, &str)>) -> (String, bool) {
        let store = store_with_secret();
        let mut tty = FakeTty::replying("typed-pw");
        let got = answer(
            prompt,
            None,
            Some(ID),
            expected,
            OneTime::NotUsed,
            &store,
            &mut tty,
        )
        .unwrap();
        let asked = !tty.asked.is_empty();
        if asked {
            // Whatever reaches the terminal is a password prompt: hidden.
            assert_eq!(tty.asked, [(prompt.to_owned(), false)], "{prompt:?}");
        }
        (got.as_str().to_owned(), asked)
    }

    #[test]
    fn proxyjump_hop_prompt_goes_to_tty() {
        // ssh asks for the jump host's password through the same helper.
        for prompt in [
            "alice@jump.example.com's password: ",
            "(alice@jump.example.com) Password: ",
            "alice@web1.example.com.evil.net's password: ",
        ] {
            assert_eq!(
                answered_for(prompt, EXPECTED),
                ("typed-pw".to_owned(), true)
            );
        }
    }

    #[test]
    fn wrong_user_goes_to_tty() {
        for prompt in [
            "bob@web1.example.com's password: ",
            "(bob@web1.example.com) Password: ",
            "root@web1.example.com's password: ",
        ] {
            assert_eq!(
                answered_for(prompt, EXPECTED),
                ("typed-pw".to_owned(), true)
            );
        }
    }

    #[test]
    fn wrong_host_goes_to_tty() {
        for prompt in [
            "alice@web2.example.com's password: ",
            "(alice@web2.example.com) Password: ",
            "alice@web1's password: ",
        ] {
            assert_eq!(
                answered_for(prompt, EXPECTED),
                ("typed-pw".to_owned(), true)
            );
        }
    }

    #[test]
    fn identity_comparison_is_case_sensitive() {
        for prompt in [
            "Alice@web1.example.com's password: ",
            "alice@WEB1.example.com's password: ",
            "(ALICE@web1.example.com) Password: ",
        ] {
            assert_eq!(
                answered_for(prompt, EXPECTED),
                ("typed-pw".to_owned(), true)
            );
        }
    }

    #[test]
    fn bare_password_prompt_goes_to_tty() {
        for prompt in ["Password: ", "password:", "  Password:", "Enter password: "] {
            assert_eq!(
                answered_for(prompt, EXPECTED),
                ("typed-pw".to_owned(), true)
            );
        }
    }

    #[test]
    fn new_password_prompt_goes_to_tty() {
        for prompt in [
            "New password: ",
            "Retype new password: ",
            "(current) UNIX password: ",
            "(alice@web1.example.com) New password: ",
            "Current password for alice@web1.example.com's password: ",
        ] {
            assert_eq!(
                answered_for(prompt, EXPECTED),
                ("typed-pw".to_owned(), true)
            );
        }
    }

    #[test]
    fn no_expected_identity_never_auto_answers() {
        for prompt in PASSWORD_PROMPTS {
            assert_eq!(answered_for(prompt, None), ("typed-pw".to_owned(), true));
        }
        // An empty identity is no identity.
        for expected in [Some(("", "")), Some(("alice", "")), Some(("", HOST))] {
            for prompt in [
                "@'s password: ",
                "alice@'s password: ",
                "(@) Password: ",
                "@web1.example.com's password: ",
            ] {
                assert_eq!(
                    answered_for(prompt, expected),
                    ("typed-pw".to_owned(), true)
                );
            }
        }
    }

    #[test]
    fn plain_prompt_with_matching_identity_is_answered() {
        for prompt in [
            "alice@web1.example.com's password:",
            "alice@web1.example.com's password: ",
            "alice@web1.example.com's password:\t\n",
        ] {
            assert_eq!(
                answered_for(prompt, EXPECTED),
                (SECRET.to_owned(), false),
                "{prompt:?}"
            );
        }
    }

    #[test]
    fn keyboard_interactive_form_matches_case_insensitive_word() {
        let expected = Some(("alice", "web1"));
        for prompt in [
            "(alice@web1) Password:",
            "(alice@web1) password: ",
            "(alice@web1) PASSWORD:",
        ] {
            assert_eq!(
                answered_for(prompt, expected),
                (SECRET.to_owned(), false),
                "{prompt:?}"
            );
        }
    }

    #[test]
    fn host_with_regex_like_chars_matches_literally() {
        let expected = Some(("alice", "a.b"));
        assert_eq!(
            answered_for("alice@a.b's password: ", expected),
            (SECRET.to_owned(), false)
        );
        assert_eq!(
            answered_for("alice@axb's password: ", expected),
            ("typed-pw".to_owned(), true)
        );
        assert_eq!(
            answered_for("(alice@axb) Password: ", expected),
            ("typed-pw".to_owned(), true)
        );

        let expected = Some(("a+", "h*"));
        assert_eq!(
            answered_for("a+@h*'s password: ", expected),
            (SECRET.to_owned(), false)
        );
        assert_eq!(
            answered_for("aa@hhh's password: ", expected),
            ("typed-pw".to_owned(), true)
        );
    }

    #[test]
    fn fido_pin_prompt_is_not_echoed() {
        let store = MemorySecretStore::default();
        let prompt = "Enter PIN for ECDSA-SK key /home/u/.ssh/id_ecdsa_sk: ";
        let mut tty = FakeTty::replying("1234");
        let got = answer(
            prompt,
            None,
            Some(ID),
            EXPECTED,
            OneTime::NotUsed,
            &store,
            &mut tty,
        )
        .unwrap();
        assert_eq!(got.as_str(), "1234");
        assert_eq!(tty.asked, [(prompt.to_owned(), false)]);
    }

    #[test]
    fn none_prompt_notifies_and_returns_empty() {
        let store = store_with_secret();
        let mut tty = FakeTty::replying("unused");
        let prompt = "Confirm user presence for key ED25519-SK SHA256:abc";
        let got = answer(
            prompt,
            Some("none"),
            Some(ID),
            EXPECTED,
            OneTime::NotUsed,
            &store,
            &mut tty,
        )
        .unwrap();
        assert_eq!(got.as_str(), "");
        assert_eq!(tty.notified, [prompt.to_owned()]);
        assert!(tty.asked.is_empty(), "display-only messages read no input");
    }

    #[test]
    fn none_prompt_that_looks_like_a_password_prompt_is_only_displayed() {
        let store = store_with_secret();
        let mut tty = FakeTty::replying("unused");
        let got = answer(
            PASSWORD_PROMPTS[0],
            Some("none"),
            Some(ID),
            EXPECTED,
            OneTime::NotUsed,
            &store,
            &mut tty,
        )
        .unwrap();
        assert_eq!(got.as_str(), "");
        assert!(tty.asked.is_empty());
    }

    #[test]
    fn no_tty_is_an_error() {
        let store = MemorySecretStore::default();
        // Nothing stored, so a password prompt needs the terminal.
        let mut tty = FakeTty::default();
        assert!(
            answer(
                PASSWORD_PROMPTS[0],
                None,
                Some(ID),
                EXPECTED,
                OneTime::NotUsed,
                &store,
                &mut tty
            )
            .is_err()
        );
        // Host-key confirmation and passphrases need it regardless of the store.
        let store = store_with_secret();
        let mut tty = FakeTty::default();
        assert!(
            answer(
                HOST_KEY_PROMPT,
                Some("confirm"),
                Some(ID),
                EXPECTED,
                OneTime::NotUsed,
                &store,
                &mut tty
            )
            .is_err()
        );
        assert!(
            answer(
                PASSPHRASE_PROMPT,
                None,
                Some(ID),
                EXPECTED,
                OneTime::NotUsed,
                &store,
                &mut tty
            )
            .is_err()
        );
        // Even a display-only message fails: ssh must then fail closed.
        assert!(
            answer(
                "note",
                Some("none"),
                Some(ID),
                EXPECTED,
                OneTime::NotUsed,
                &store,
                &mut tty
            )
            .is_err()
        );
    }

    #[test]
    fn echo_is_off_for_secret_looking_prompts_and_on_otherwise() {
        let store = MemorySecretStore::default();
        let cases = [
            ("Enter passphrase for key 'k': ", false),
            ("Password: ", false),
            ("Verification code: ", false),
            ("Enter Authenticator CODE: ", false),
            (
                "Enter PIN for ECDSA-SK key /home/u/.ssh/id_ecdsa_sk: ",
                false,
            ),
            ("Enter your Pin: ", false),
            ("Enter your name: ", true),
            ("Spinner name: ", true),
            ("Are you sure (yes/no)? ", true),
        ];
        for (prompt, echo) in cases {
            let mut tty = FakeTty::replying("x");
            answer(prompt, None, None, None, OneTime::NotUsed, &store, &mut tty).unwrap();
            assert_eq!(tty.asked, [(prompt.to_owned(), echo)], "prompt {prompt:?}");
        }
    }

    // --- The one-time marker --------------------------------------------------

    /// The token of the connection under test, and of another one.
    const TOKEN: &str = "6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b";
    const OTHER_TOKEN: &str = "0b9a8c7d-6e5f-4a3b-9c2d-1e0f9a8b7c6d";

    /// The permission bits of `path` itself (a symlink is not followed).
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777
    }

    /// The names in `dir`, sorted.
    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// `answer` for `prompt` with the store holding `SECRET` and the user typing
    /// "typed-pw" if asked, with `claim` as the one-time claim. Returns the
    /// answer and the terminal, to see what it was asked.
    fn answered_once(
        prompt: &str,
        env: Option<&str>,
        claim: &dyn Fn() -> bool,
    ) -> (String, FakeTty) {
        let store = store_with_secret();
        let mut tty = FakeTty::replying("typed-pw");
        let got = answer(
            prompt,
            env,
            Some(ID),
            EXPECTED,
            OneTime::Claim(claim),
            &store,
            &mut tty,
        )
        .unwrap();
        (got.as_str().to_owned(), tty)
    }

    #[test]
    fn a_token_is_a_fresh_lowercase_canonical_uuid() {
        let a = AskpassToken::new();
        let b = AskpassToken::new();
        assert_ne!(a, b);
        for token in [&a, &b] {
            assert!(is_canonical_uuid(token.as_str()), "{token:?}");
            assert_eq!(token.as_str(), token.as_str().to_lowercase());
        }
        assert_eq!(TOKEN_ENV, "MONIKER_ASKPASS_TOKEN");
    }

    #[test]
    fn the_marker_dir_is_moniker_askpass_in_the_runtime_dir() {
        assert_eq!(
            marker_dir(Path::new("/run/user/1000")),
            Path::new("/run/user/1000/moniker/askpass")
        );
    }

    #[test]
    fn claim_is_true_once_then_false() {
        let rt = tempfile::tempdir().unwrap();
        assert!(claim_marker(rt.path(), TOKEN).unwrap());
        assert!(!claim_marker(rt.path(), TOKEN).unwrap());
        assert!(!claim_marker(rt.path(), TOKEN).unwrap());
        // Another connection has a marker of its own.
        assert!(claim_marker(rt.path(), OTHER_TOKEN).unwrap());
        assert_eq!(names_in(&marker_dir(rt.path())), [OTHER_TOKEN, TOKEN]);
    }

    #[test]
    fn claim_rejects_a_non_uuid_token() {
        let rt = tempfile::tempdir().unwrap();
        let upper = TOKEN.to_uppercase();
        let braced = format!("{{{TOKEN}}}");
        for token in [
            "../x",
            "x",
            upper.as_str(),
            "",
            ".",
            "..",
            "/tmp/x",
            braced.as_str(),
            "6f1c2a3b4d5e4f608a7b9c0d1e2f3a4b",
        ] {
            assert!(claim_marker(rt.path(), token).is_err(), "{token:?}");
            release_marker(rt.path(), token);
        }
        // Rejected before anything was made.
        assert!(names_in(rt.path()).is_empty(), "{:?}", names_in(rt.path()));
    }

    #[test]
    fn claim_refuses_a_relative_runtime_dir() {
        for dir in ["", "relative-runtime-dir", "./relative-runtime-dir"] {
            assert!(claim_marker(Path::new(dir), TOKEN).is_err(), "{dir:?}");
            release_marker(Path::new(dir), TOKEN);
        }
        assert!(!Path::new("relative-runtime-dir").exists());
        assert!(!Path::new("moniker").exists());
    }

    #[test]
    fn marker_files_are_0600_and_dirs_0700() {
        let rt = tempfile::tempdir().unwrap();
        assert!(claim_marker(rt.path(), TOKEN).unwrap());
        assert_eq!(mode_of(&rt.path().join("moniker")), 0o700);
        assert_eq!(mode_of(&marker_dir(rt.path())), 0o700);
        let marker = marker_dir(rt.path()).join(TOKEN);
        assert_eq!(mode_of(&marker), 0o600);
        assert_eq!(
            std::fs::metadata(&marker).unwrap().len(),
            0,
            "it holds nothing"
        );
    }

    /// Set the permission bits of `path` (whatever the umask made them).
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// A new directory at `path` that passes every check but the one under
    /// test: a real directory of this user's, mode 0700.
    fn private_dir_at(path: &Path) {
        std::fs::create_dir(path).unwrap();
        set_mode(path, 0o700);
    }

    #[test]
    fn a_symlinked_marker_dir_is_refused() {
        // The links point at a private directory of this user's, so only the
        // refusal to follow a link stops them.
        let elsewhere = tempfile::tempdir().unwrap();
        set_mode(elsewhere.path(), 0o700);

        // `moniker/askpass` is a link to a directory elsewhere.
        let rt = tempfile::tempdir().unwrap();
        private_dir_at(&rt.path().join("moniker"));
        std::os::unix::fs::symlink(elsewhere.path(), marker_dir(rt.path())).unwrap();
        assert!(claim_marker(rt.path(), TOKEN).is_err());
        assert!(
            names_in(elsewhere.path()).is_empty(),
            "nothing at the link's target"
        );

        // `moniker` itself is a link.
        let rt = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), rt.path().join("moniker")).unwrap();
        assert!(claim_marker(rt.path(), TOKEN).is_err());
        assert!(
            names_in(elsewhere.path()).is_empty(),
            "nothing at the link's target"
        );

        // A dangling link is not followed to create its target either.
        let rt = tempfile::tempdir().unwrap();
        let target = elsewhere.path().join("made-by-following");
        std::os::unix::fs::symlink(&target, rt.path().join("moniker")).unwrap();
        assert!(claim_marker(rt.path(), TOKEN).is_err());
        assert!(!target.exists());

        // The marker's own name as a link (to a file that does not exist yet):
        // already there, so no claim, and nothing made at its target.
        let rt = tempfile::tempdir().unwrap();
        private_dir_at(&rt.path().join("moniker"));
        private_dir_at(&marker_dir(rt.path()));
        std::os::unix::fs::symlink(&target, marker_dir(rt.path()).join(TOKEN)).unwrap();
        assert!(!claim_marker(rt.path(), TOKEN).unwrap());
        assert!(!target.exists());

        // Releasing does not follow a link to remove a file over there.
        let rt = tempfile::tempdir().unwrap();
        private_dir_at(&rt.path().join("moniker"));
        std::os::unix::fs::symlink(elsewhere.path(), marker_dir(rt.path())).unwrap();
        std::fs::write(elsewhere.path().join(TOKEN), "").unwrap();
        release_marker(rt.path(), TOKEN);
        assert!(elsewhere.path().join(TOKEN).exists());
    }

    #[test]
    fn a_marker_dir_that_is_not_private_is_refused() {
        // Group or other bits on either directory.
        for (moniker_mode, askpass_mode) in [(0o755, 0o700), (0o701, 0o700), (0o700, 0o770)] {
            let rt = tempfile::tempdir().unwrap();
            assert!(claim_marker(rt.path(), OTHER_TOKEN).unwrap());
            set_mode(&rt.path().join("moniker"), moniker_mode);
            set_mode(&marker_dir(rt.path()), askpass_mode);
            assert!(
                claim_marker(rt.path(), TOKEN).is_err(),
                "{moniker_mode:o}/{askpass_mode:o}"
            );
            assert!(!marker_dir(rt.path()).join(TOKEN).exists());
            // And a release there removes nothing.
            release_marker(rt.path(), OTHER_TOKEN);
            assert!(marker_dir(rt.path()).join(OTHER_TOKEN).exists());
        }
        // A file where a directory belongs.
        let rt = tempfile::tempdir().unwrap();
        std::fs::write(rt.path().join("moniker"), "").unwrap();
        assert!(claim_marker(rt.path(), TOKEN).is_err());
        let rt = tempfile::tempdir().unwrap();
        private_dir_at(&rt.path().join("moniker"));
        std::fs::write(marker_dir(rt.path()), "").unwrap();
        assert!(claim_marker(rt.path(), TOKEN).is_err());
    }

    #[test]
    fn an_existing_private_marker_dir_is_used() {
        use std::os::unix::fs::DirBuilderExt;
        let rt = tempfile::tempdir().unwrap();
        std::fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(marker_dir(rt.path()))
            .unwrap();
        assert!(claim_marker(rt.path(), TOKEN).unwrap());
    }

    #[test]
    fn release_removes_the_marker() {
        let rt = tempfile::tempdir().unwrap();
        assert!(claim_marker(rt.path(), TOKEN).unwrap());
        assert!(claim_marker(rt.path(), OTHER_TOKEN).unwrap());
        release_marker(rt.path(), TOKEN);
        assert_eq!(
            names_in(&marker_dir(rt.path())),
            [OTHER_TOKEN],
            "only its own"
        );
        // Releasing again, or with nothing ever made, is harmless.
        release_marker(rt.path(), TOKEN);
        release_marker(tempfile::tempdir().unwrap().path(), TOKEN);
    }

    #[test]
    fn a_second_password_prompt_goes_to_the_terminal() {
        let rt = tempfile::tempdir().unwrap();
        let claim = || claim_marker(rt.path(), TOKEN).unwrap_or(false);

        let (first, tty) = answered_once(PASSWORD_PROMPTS[0], None, &claim);
        assert_eq!(first, SECRET);
        assert!(tty.untouched());

        // The server rejected it and asks again, in either form: the stored
        // password is not sent a second time; the user types, unseen.
        for prompt in PASSWORD_PROMPTS {
            let (again, tty) = answered_once(prompt, None, &claim);
            assert_eq!(again, "typed-pw", "{prompt:?}");
            assert_eq!(tty.asked, [(prompt.to_owned(), false)]);
        }
    }

    #[test]
    fn a_host_key_question_does_not_use_the_marker() {
        let rt = tempfile::tempdir().unwrap();
        let claims = Cell::new(0);
        let claim = || {
            claims.set(claims.get() + 1);
            claim_marker(rt.path(), TOKEN).unwrap_or(false)
        };

        // The first connection to a host: its host key is asked about first.
        let (yes, tty) = answered_once(HOST_KEY_PROMPT, Some("confirm"), &claim);
        assert_eq!(yes, "typed-pw", "the user's own answer");
        assert_eq!(tty.asked, [(HOST_KEY_PROMPT.to_owned(), true)]);
        // Nor do the same question without the env marker, or a notice.
        let (no, _) = answered_once(HOST_KEY_PROMPT, None, &claim);
        assert_eq!(no, "typed-pw");
        let (shown, tty) = answered_once("Warning: Permanently added", Some("none"), &claim);
        assert_eq!(shown, "");
        assert_eq!(tty.notified.len(), 1);
        assert_eq!(claims.get(), 0, "no question so far could use the marker");

        // Then the password prompt: it claims the marker and gets the secret.
        let (password, tty) = answered_once(PASSWORD_PROMPTS[0], None, &claim);
        assert_eq!(password, SECRET);
        assert!(tty.untouched());
        assert_eq!(claims.get(), 1);
    }

    #[test]
    fn a_prompt_for_another_host_never_claims() {
        let claims = Cell::new(0);
        let claim = || {
            claims.set(claims.get() + 1);
            true
        };
        for prompt in [
            "bob@jump's password: ",
            "(bob@jump) Password: ",
            "alice@jump.example.com's password: ",
            "Password: ",
            PASSPHRASE_PROMPT,
        ] {
            let (got, tty) = answered_once(prompt, None, &claim);
            assert_eq!(got, "typed-pw", "{prompt:?}");
            assert_eq!(tty.asked, [(prompt.to_owned(), false)]);
        }
        assert_eq!(claims.get(), 0);

        // Nor does the profile's own prompt when nothing is stored for it, or
        // when the profile id cannot be used.
        let mut tty = FakeTty::replying("typed-pw");
        let empty = MemorySecretStore::default();
        answer(
            PASSWORD_PROMPTS[0],
            None,
            Some(ID),
            EXPECTED,
            OneTime::Claim(&claim),
            &empty,
            &mut tty,
        )
        .unwrap();
        let mut tty = FakeTty::replying("typed-pw");
        answer(
            PASSWORD_PROMPTS[0],
            None,
            Some("../x"),
            EXPECTED,
            OneTime::Claim(&claim),
            &store_with_secret(),
            &mut tty,
        )
        .unwrap();
        assert_eq!(claims.get(), 0);
    }

    #[test]
    fn no_runtime_dir_means_the_terminal() {
        // What the helper passes when the marker cannot be made at all.
        for prompt in PASSWORD_PROMPTS {
            let (got, tty) = answered_once(prompt, None, &|| false);
            assert_eq!(got, "typed-pw");
            assert_eq!(tty.asked, [(prompt.to_owned(), false)]);
        }
    }

    #[test]
    fn not_used_keeps_todays_behavior() {
        // The sync path: no marker, and ssh itself allows one prompt.
        let store = store_with_secret();
        for prompt in [
            PASSWORD_PROMPTS[0],
            PASSWORD_PROMPTS[0],
            PASSWORD_PROMPTS[1],
        ] {
            let mut tty = FakeTty::default();
            let got = answer(
                prompt,
                None,
                Some(ID),
                EXPECTED,
                OneTime::NotUsed,
                &store,
                &mut tty,
            )
            .unwrap();
            assert_eq!(got.as_str(), SECRET);
            assert!(tty.untouched());
        }
    }

    // --- DevTty building blocks, against a pseudo-terminal -------------------

    use rustix::fs::{OFlags, fcntl_setfl};
    use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
    use rustix::termios::{LocalModes, tcgetattr};
    use std::ffi::OsStr;
    use std::fs::OpenOptions;
    use std::io::Cursor;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    use std::time::Duration;

    /// A pseudo-terminal: (master side, slave side). The slave is the terminal
    /// the code under test sees; the master is the "screen and keyboard".
    fn pty() -> (File, File) {
        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).expect("open a pty");
        grantpt(&master).unwrap();
        unlockpt(&master).unwrap();
        let name = ptsname(&master, Vec::new()).unwrap();
        let slave = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(OFlags::NOCTTY.bits() as i32)
            .open(OsStr::from_bytes(name.to_bytes()))
            .unwrap();
        (File::from(master), slave)
    }

    /// Everything the terminal has shown so far (the master's output).
    fn screen(master: &File) -> String {
        fcntl_setfl(master, OFlags::NONBLOCK).unwrap();
        let mut shown = Vec::new();
        let mut reader = master;
        let mut buf = [0u8; 256];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => shown.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("reading the pty master: {e}"),
            }
        }
        String::from_utf8(shown).unwrap()
    }

    fn echo_is_on(tty: &File) -> bool {
        tcgetattr(tty)
            .unwrap()
            .local_modes
            .contains(LocalModes::ECHO)
    }

    #[test]
    fn hidden_input_is_not_echoed_and_echo_is_restored() {
        let (master, slave) = pty();
        assert!(echo_is_on(&slave));
        let typist = {
            let slave = slave.try_clone().unwrap();
            let mut keyboard = master.try_clone().unwrap();
            std::thread::spawn(move || {
                // Type only once echo is off: switching it off discards earlier typeahead.
                for _ in 0..1000 {
                    if !echo_is_on(&slave) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                keyboard.write_all(b"hunter2\n").unwrap();
            })
        };
        let got = ask_on(&slave, "Password: ", false).unwrap();
        typist.join().unwrap();

        assert_eq!(got, "hunter2");
        assert!(echo_is_on(&slave), "echo must be restored");
        let shown = screen(&master);
        assert!(
            shown.starts_with("Password: "),
            "prompt shown, got {shown:?}"
        );
        assert!(!shown.contains("hunter2"), "typed text leaked: {shown:?}");
    }

    #[test]
    fn visible_input_is_echoed() {
        let (master, slave) = pty();
        let typist = {
            let mut keyboard = master.try_clone().unwrap();
            std::thread::spawn(move || {
                // Type once the prompt is up, so the echo cannot interleave with it.
                let mut seen = String::new();
                for _ in 0..1000 {
                    seen.push_str(&screen(&keyboard));
                    if seen.contains("Name: ") {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                keyboard.write_all(b"visible\n").unwrap();
                seen
            })
        };
        let got = ask_on(&slave, "Name: ", true).unwrap();
        let before_typing = typist.join().unwrap();

        assert_eq!(got, "visible");
        assert!(echo_is_on(&slave));
        assert_eq!(before_typing, "Name: ");
        let echoed = screen(&master);
        assert!(echoed.contains("visible"), "{echoed:?}");
    }

    #[test]
    fn hidden_input_on_a_non_terminal_fails_closed() {
        // Cannot switch echo off, so nothing is shown and nothing is read.
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"typed\n").unwrap();
        assert!(ask_on(&file, "Password: ", false).is_err());
        assert_eq!(
            file.metadata().unwrap().len(),
            6,
            "no prompt may be written"
        );
    }

    #[test]
    fn read_line_strips_only_the_newline() {
        let mut input = Cursor::new(b"first line\nsecond\n".to_vec());
        assert_eq!(read_line(&mut input).unwrap(), "first line");
        assert_eq!(read_line(&mut input).unwrap(), "second");
        assert_eq!(read_line(&mut Cursor::new(b"\n".to_vec())).unwrap(), "");
        assert_eq!(
            read_line(&mut Cursor::new(b"no newline".to_vec())).unwrap(),
            "no newline"
        );
        assert_eq!(
            read_line(&mut Cursor::new(b"a b\r\n".to_vec())).unwrap(),
            "a b\r"
        );
    }

    #[test]
    fn read_line_at_end_of_input_is_an_error() {
        let err = read_line(&mut Cursor::new(Vec::new())).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    // ----- The effective identity (`ssh -G`) -----

    /// Part of what `ssh -G -p 22 -- alice@web` prints, as OpenSSH 10.2 does:
    /// lowercase keys, the host lowercased, some keys repeated.
    const DUMP: &str = "user alice\nhostname web1.example.com\nport 22\n\
        identityfile ~/.ssh/id_rsa\nidentityfile ~/.ssh/id_ed25519\nsendenv LANG\n\
        sendenv LC_*\n";

    #[test]
    fn the_effective_identity_is_the_user_and_host_ssh_prints() {
        assert_eq!(
            effective_identity(DUMP, "alice"),
            Some(("alice".to_owned(), "web1.example.com".to_owned()))
        );
    }

    #[test]
    fn a_host_key_alias_is_the_host_the_prompt_names() {
        let dump = format!("{DUMP}hostkeyalias myalias\n");
        assert_eq!(
            effective_identity(&dump, "alice"),
            Some(("alice".to_owned(), "myalias".to_owned()))
        );
    }

    #[test]
    fn another_user_than_the_profiles_is_not_trusted() {
        let dump = DUMP.replace("user alice", "user bob");
        assert_eq!(effective_identity(&dump, "alice"), None);
    }

    #[test]
    fn a_dump_without_its_user_or_host_is_not_trusted() {
        assert_eq!(effective_identity("", "alice"), None);
        assert_eq!(effective_identity("user alice\nport 22\n", "alice"), None);
        assert_eq!(
            effective_identity("hostname web1\nport 22\n", "alice"),
            None
        );
        // Keys are matched whole and as ssh prints them, in lowercase.
        assert_eq!(
            effective_identity("user alice\nhostnames web1\n", "alice"),
            None
        );
        assert_eq!(
            effective_identity("user alice\nHostName web1\n", "alice"),
            None
        );
    }

    #[test]
    fn a_repeated_identity_key_is_ambiguous() {
        for extra in [
            "hostname other.example.com",
            "user alice",
            "hostkeyalias a\nhostkeyalias b",
        ] {
            let dump = format!("{DUMP}{extra}\n");
            assert_eq!(effective_identity(&dump, "alice"), None, "{extra}");
        }
    }

    #[test]
    fn odd_identity_values_are_not_trusted() {
        for host in ["web1 example.com", "web1\u{7}", "", " web1", "web1\t"] {
            let dump = format!("user alice\nhostname {host}\n");
            assert_eq!(effective_identity(&dump, "alice"), None, "{host:?}");
        }
        assert_eq!(effective_identity("user alice\nhostname\n", "alice"), None);
        let dump = format!("{DUMP}hostkeyalias my alias\n");
        assert_eq!(effective_identity(&dump, "alice"), None);
    }

    #[test]
    fn a_login_prompt_is_told_apart_before_the_host_is_known() {
        for prompt in [
            "alice@web1.example.com's password: ",
            "(alice@web1.example.com) Password: ",
            "alice@another.example.com's password:",
            "(alice@x) PASSWORD:  ",
        ] {
            assert!(could_be_login_prompt(prompt, USER), "{prompt:?}");
        }
        for prompt in [
            PASSPHRASE_PROMPT,
            HOST_KEY_PROMPT,
            "Password: ",
            "bob@web1.example.com's password: ",
            "alicex@web1.example.com's password: ",
            "alice@web1.example.com's password: and more",
            "Verification code: ",
            "(alice@web1.example.com) Verification code: ",
        ] {
            assert!(!could_be_login_prompt(prompt, USER), "{prompt:?}");
        }
        assert!(!could_be_login_prompt("@web1's password: ", ""));
    }

    // ----- Who is asking -----

    #[test]
    fn the_parent_is_the_fourth_field_of_a_stat_line() {
        assert_eq!(
            parent_in_stat("4321 (ssh) S 1234 4321 4321 0 -1 4194560\n"),
            Some(1234)
        );
        // The command name may hold spaces and parentheses; only the last `)` ends it.
        assert_eq!(parent_in_stat("4321 (a) b) S 9 (c) R 77 1 1\n"), Some(77));
        assert_eq!(parent_in_stat("4321 (we ird) R 77 1 1\n"), Some(77));
        for bad in [
            "",
            "garbage",
            "4321 (ssh) S",
            "4321 (ssh) S -5 1",
            "4321 (ssh) S x 1",
        ] {
            assert_eq!(parent_in_stat(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn only_the_ssh_moniker_started_is_trusted() {
        assert!(started_by_moniker(Some("1234"), Some(1234)));
        // A ProxyJump or ProxyCommand hop's ssh is a child of the first ssh.
        assert!(!started_by_moniker(Some("1234"), Some(1240)));
        assert!(!started_by_moniker(None, Some(1234)));
        assert!(!started_by_moniker(Some("1234"), None));
        // An orphaned ssh's parent is init (or a subreaper): never Moniker.
        for odd in [
            "0",
            "1",
            "",
            "+1234",
            " 1234",
            "1234 ",
            "12a",
            "99999999999",
        ] {
            let parent = odd.trim().parse::<u32>().ok();
            assert!(!started_by_moniker(Some(odd), parent), "{odd:?}");
        }
    }

    #[test]
    fn read_line_rejects_invalid_utf8() {
        let err = read_line(&mut Cursor::new(vec![0xff, 0xfe, b'\n'])).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn notify_writes_message_and_newline() {
        let mut out = Vec::new();
        notify_on(&mut out, "Confirm user presence").unwrap();
        assert_eq!(out, b"Confirm user presence\n");
    }

    #[test]
    fn terminal_escape_sequences_in_prompts_are_defused() {
        // Prompt text can come from the remote server (keyboard-interactive).
        let hostile = "ok\x1b[2J\x07\u{9b}31m\tstay\nline";
        let shown = printable(hostile);
        assert!(
            !shown
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t'),
            "{shown:?}"
        );
        assert!(shown.starts_with("ok") && shown.contains("\tstay\nline"));
        let mut out = Vec::new();
        notify_on(&mut out, hostile).unwrap();
        assert!(!out.contains(&0x1b));
    }
}
