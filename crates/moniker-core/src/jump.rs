//! Jump hosts: which profile another profile goes through, and the shared
//! connection a connection through it uses.
//!
//! A profile names its jump host by id (`Profile::jump_host`). The first part
//! of this module only reads profiles: where a profile's connection goes
//! ([`route`]) and who is affected when a profile is picked or deleted as
//! somebody's jump host. Moniker uses one jump host only, and never a direct
//! connection the person did not choose: a profile whose jump host is gone or
//! has a jump host of its own is refused, with a text that says why.
//!
//! The second part is the shared connection (the commands are in
//! `crate::ssh`: `jump_master_command`, `hop_command`, `jump_control_command`;
//! [`pane_commands`] builds a terminal pane's together, for the GUI and the
//! end-to-end check alike). Each connection through a jump host has a master
//! of its own, an `ssh` that signed in to the jump host and went to the
//! background, listening on a socket nobody else can reach: [`JumpSocket`], a
//! new random name in `<runtime>/moniker/jump/`, a folder private to this user
//! (checked like the askpass markers' folder) in a runtime folder that is this
//! user's and that nobody else may write in. The socket's path goes into the
//! hop's shell string, so it is made only of `[A-Za-z0-9/._-]` and is at most
//! [`MAX_SOCKET_PATH`] bytes long; a runtime folder that does not allow that is
//! refused rather than quoted. No TCP port is opened.
//!
//! The master is no child of Moniker once it went to the background (`-f`), so
//! its process id is asked of it: `ssh -O check` prints it ([`master_pid`],
//! through [`run_control`]). [`learn_master`] asks, holds the process by a
//! pidfd ([`open_master`]) and asks again, and [`end_master`] ends it at once
//! with `SIGTERM` (which also removes the socket), a system call that reads no
//! configuration and so works as the application quits. Without the pidfd,
//! `ssh -O exit` does the same through the socket. Should neither happen, the
//! master ends by itself [`PERSIST_SECS`] after its last channel closed
//! (`ControlPersist`).

use std::io::{self, Read};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use rustix::fs::{AtFlags, fstat, statat};
use rustix::io::Errno;
use rustix::process::{Pid, PidfdFlags, Signal, geteuid, pidfd_open, pidfd_send_signal};
use uuid::Uuid;

use crate::askpass::{AskpassToken, RUNTIME_SUBDIR, moniker_subdir, open_runtime_dir};
use crate::profile::{Profile, StartupSequence};
use crate::ssh::{
    JumpControl, SshCommand, hop_command, jump_control_command, jump_master_command,
    jump_sync_master_command, terminal_command,
};
use crate::ssh_config::HELPER_ENV;

/// How long a master stays after its last channel closed (`ControlPersist`): a
/// backstop for a master Moniker could not end, and the time the target's
/// `ssh` has to attach once the master signed in.
pub const PERSIST_SECS: u64 = 10;

/// The longest path a [`JumpSocket`] may have, in bytes. ssh binds the socket
/// under a temporary name 17 bytes longer before it renames it, and a socket's
/// path has 108 bytes with its NUL (`sun_path`).
pub const MAX_SOCKET_PATH: usize = 90;

/// How long `ssh -O check` or `ssh -O exit` may take ([`run_control`]). They
/// only talk to the master over the socket and answer in milliseconds.
pub const CONTROL_LIMIT: Duration = Duration::from_secs(5);

/// The most of a control command's stderr [`run_control`] keeps: its end.
const CONTROL_MAX_BYTES: usize = 4096;

/// The folder of the sockets in Moniker's runtime folder:
/// `<runtime>/moniker/jump`.
const SOCKET_SUBDIR: &str = "jump";

/// The refusal for a profile whose jump host is not saved (here), as a tab or a
/// page says it.
pub const MISSING_TEXT: &str = "This profile's jump host is no longer saved.";

/// The refusal for a profile whose jump host has a jump host of its own.
/// `name` is the name of that jump host, the one that goes through another.
pub fn chain_text(name: &str) -> String {
    format!("{name} goes through a jump host of its own. Moniker uses only one jump host.")
}

/// Where a profile's connection goes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Route<'a> {
    /// Straight to its own host: no jump host is set.
    Direct,
    /// Through this profile, which has no jump host of its own.
    Through(&'a Profile),
    /// A jump host is set and nothing live has its id.
    Missing,
    /// Through this profile, which goes through a jump host of its own.
    Chained(&'a Profile),
}

impl<'a> Route<'a> {
    /// Why the connection must not start, when it must not: `Missing` and
    /// `Chained` never fall back to a direct connection.
    pub fn refusal(&self) -> Option<String> {
        match self {
            Route::Direct | Route::Through(_) => None,
            Route::Missing => Some(MISSING_TEXT.to_string()),
            Route::Chained(jump) => Some(chain_text(&jump.name)),
        }
    }

    /// The profile the connection goes through, if one is saved: `Through` and
    /// `Chained` have one, though a `Chained` one is refused. It is borrowed
    /// from the profiles the route was found in, so it outlives the route.
    pub fn jump(&self) -> Option<&'a Profile> {
        match *self {
            Route::Through(jump) | Route::Chained(jump) => Some(jump),
            Route::Direct | Route::Missing => None,
        }
    }
}

/// Where `p`'s connection goes, with `find` looking up a profile by id
/// (`ProfileStore::get` gives live profiles only; a tombstone that `find` does
/// give is treated as not saved).
pub fn route<'a>(p: &Profile, find: impl Fn(&str) -> Option<&'a Profile>) -> Route<'a> {
    let Some(id) = &p.jump_host else {
        return Route::Direct;
    };
    match find(id).filter(|jump| !jump.deleted) {
        None => Route::Missing,
        Some(jump) if jump.jump_host.is_some() => Route::Chained(jump),
        Some(jump) => Route::Through(jump),
    }
}

/// The live profiles that go through the profile `id`, by name without regard
/// to case, then by id. The question deleting a jump host asks, and what a
/// profile that others go through is grayed out for.
pub fn going_through<'a>(id: &str, all: impl IntoIterator<Item = &'a Profile>) -> Vec<&'a Profile> {
    in_name_order(
        all.into_iter()
            .filter(|p| !p.deleted && p.jump_host.as_deref() == Some(id)),
    )
}

/// The live profiles that can be the jump host of the profile `for_id`: not
/// itself, and none that goes through a jump host of its own. In name order
/// like [`going_through`].
pub fn usable_jump_hosts<'a>(
    for_id: &str,
    all: impl IntoIterator<Item = &'a Profile>,
) -> Vec<&'a Profile> {
    in_name_order(
        all.into_iter()
            .filter(|p| !p.deleted && p.jump_host.is_none() && p.id != for_id),
    )
}

/// Whether any live profile goes through a jump host. A sync that carries one
/// must keep out an older Moniker, which would connect it straight to its host.
pub fn holds_jump_host<'a>(all: impl IntoIterator<Item = &'a Profile>) -> bool {
    all.into_iter().any(|p| !p.deleted && p.jump_host.is_some())
}

/// `ps` by name without regard to case, then by id.
fn in_name_order<'a>(ps: impl Iterator<Item = &'a Profile>) -> Vec<&'a Profile> {
    let mut v: Vec<&Profile> = ps.collect();
    v.sort_by_cached_key(|p| (p.name.to_lowercase(), p.id.clone()));
    v
}

/// Where the master of one shared connection listens: a path that no other
/// connection uses, in a folder private to this user (module docs). ssh makes
/// the socket itself when the master signs in, and removes it when the master
/// ends, unless it is killed outright.
///
/// One socket per master, never reused: a new connection makes a new one
/// ([`JumpSocket::new`]), and there is no way to copy one (no `Clone`), so two
/// masters can never be given the same path.
#[derive(Debug, PartialEq, Eq)]
pub struct JumpSocket {
    path: PathBuf,
}

impl JumpSocket {
    /// A new socket path in `<runtime_dir>/moniker/jump/`, named by 32
    /// lowercase hexadecimal characters (a random UUID), with the folder opened
    /// or made private (mode 0700) on the way, following no symlink below
    /// `runtime_dir` (as for the askpass markers).
    ///
    /// Refused, with nothing made:
    /// - when `runtime_dir` is not an absolute path, or the socket's path would
    ///   hold a character other than `[A-Za-z0-9/._-]` or be longer than
    ///   [`MAX_SOCKET_PATH`] bytes: the path goes into the hop's shell string
    ///   unquoted (`crate::ssh::hop_command`);
    /// - when `runtime_dir` is not this user's, or the group or others may
    ///   write in it: someone else could then move `moniker` aside and put a
    ///   folder of theirs in its place between this check and ssh's use of the
    ///   path.
    ///
    /// Refused too when a folder on the way is not private (a symlink, someone
    /// else's, open to others, not a folder), and when something is already at
    /// the socket's path: a master started there would not listen, run on as no
    /// master at all, and be out of reach of both `ControlPersist` and `-O`.
    pub fn new(runtime_dir: &Path) -> io::Result<JumpSocket> {
        Self::named(runtime_dir, &Uuid::new_v4().simple().to_string())
    }

    /// [`new`](JumpSocket::new) with the socket's name given (tests).
    fn named(runtime_dir: &Path, name: &str) -> io::Result<JumpSocket> {
        if !runtime_dir.is_absolute() {
            return Err(refused("the runtime directory is not an absolute path"));
        }
        let path = runtime_dir
            .join(RUNTIME_SUBDIR)
            .join(SOCKET_SUBDIR)
            .join(name);
        let bytes = path.as_os_str().as_bytes();
        if !bytes.iter().all(|&b| is_socket_byte(b)) {
            return Err(refused(
                "the runtime directory's path has a character a shared connection's socket cannot have",
            ));
        }
        if bytes.len() > MAX_SOCKET_PATH {
            return Err(refused(
                "the runtime directory's path is too long for a shared connection's socket",
            ));
        }
        let runtime = open_runtime_dir(runtime_dir)?;
        let stat = fstat(&runtime)?;
        if stat.st_uid != geteuid().as_raw() || stat.st_mode & 0o022 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the runtime directory is not this user's, or others may write in it",
            ));
        }
        let dir = moniker_subdir(&runtime, SOCKET_SUBDIR, true)?;
        match statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW) {
            Err(Errno::NOENT) => Ok(JumpSocket { path }),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "something is already at the shared connection's socket path",
            )),
            Err(err) => Err(err.into()),
        }
    }

    /// The socket's path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// An error for a runtime folder that cannot hold a socket.
fn refused(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, why)
}

/// Whether `byte` may be in a socket's path: `[A-Za-z0-9/._-]`, nothing a
/// shell or ssh's `%` and `~` expansion reads. The hop's shell string checks
/// its socket with it too (`crate::ssh::hop_command`).
pub(crate) fn is_socket_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-')
}

/// The master's process id in what `ssh -O check` wrote on stderr: the number
/// in its last line that begins `Master running (`, which must be exactly
/// `Master running (pid=N)` (OpenSSH ends it with CR LF). ssh writes its answer
/// last; what comes before it (a `Match exec` command's own output) is not the
/// master's. `None` when no line says that, or the last one does not read: the
/// number not plain digits, 0, or too large.
pub fn master_pid(stderr: &str) -> Option<u32> {
    let line = stderr
        .lines()
        .rev()
        .find(|line| line.starts_with("Master running ("))?;
    let digits = line
        .strip_prefix("Master running (pid=")?
        .strip_suffix(')')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok().filter(|&pid| pid > 0)
}

/// Run a control command (`crate::ssh::jump_control_command`) and give what it
/// wrote on stderr, if it exited with status 0 within `limit`; `None` for any
/// other end. It waits for the command, so it never runs on the GUI's main
/// thread.
///
/// The command runs from argv, never through a shell, without the variables
/// that lead `ssh` to the askpass helper (only those `cmd` itself adds), with
/// no input and its standard output thrown away. Stderr is read to its end on
/// a thread of its own, so the command can never block on a full pipe, and its
/// last 4 KiB are kept (ssh writes its answer last), as text
/// with anything that is not UTF-8 replaced. A command still running at
/// `limit` is killed and reaped; one that exits but leaves a process holding
/// its stderr open is given up on at `limit` too.
pub fn run_control(cmd: &SshCommand, limit: Duration) -> Option<String> {
    let deadline = Instant::now() + limit;
    let mut command = Command::new(&cmd.program);
    command.args(&cmd.args);
    for name in HELPER_ENV {
        command.env_remove(name);
    }
    command.envs(cmd.env.iter().map(|(name, value)| (name, value)));
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let stderr = child.stderr.take()?;
    let (sender, received) = mpsc::channel();
    thread::spawn(move || {
        let _ = sender.send(tail_of(stderr));
    });
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    let tail = received
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok()?;
    Some(String::from_utf8_lossy(&tail).into_owned())
}

/// Everything `reader` gives until its end, keeping only the last
/// [`CONTROL_MAX_BYTES`]. A read error ends it with what was kept.
fn tail_of(mut reader: impl Read) -> Vec<u8> {
    let mut kept = Vec::with_capacity(2 * CONTROL_MAX_BYTES);
    let mut chunk = [0; CONTROL_MAX_BYTES];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                kept.extend_from_slice(&chunk[..n]);
                if kept.len() > CONTROL_MAX_BYTES {
                    kept.drain(..kept.len() - CONTROL_MAX_BYTES);
                }
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    kept
}

/// A pidfd for the master whose process id `-O check` gave ([`master_pid`]),
/// to end it later with [`end_master`]: it holds that very process, so a pid
/// reused after the master is gone can never be reached through it. `None` for
/// the ids 0 and 1, this process's own id, an id that does not fit, or a
/// process that is gone.
///
/// The id is the master's own answer of a moment before, and that `-O check`
/// started its [`PERSIST_SECS`] over: unless its connection to the jump host
/// broke meanwhile, it still runs when this is called right after, and the
/// kernel gives an id again only after going through the others.
pub fn open_master(pid: u32) -> Option<OwnedFd> {
    if pid == std::process::id() {
        return None;
    }
    let raw = i32::try_from(pid).ok().filter(|&raw| raw > 1)?;
    pidfd_open(Pid::from_raw(raw)?, PidfdFlags::empty()).ok()
}

/// End the master held by `pidfd` ([`open_master`]) with `SIGTERM`: it closes
/// its channels, removes its socket and exits at once. An error when it is
/// already gone.
pub fn end_master(pidfd: &OwnedFd) -> io::Result<()> {
    pidfd_send_signal(pidfd, Signal::TERM).map_err(io::Error::from)
}

/// The master `check` (`crate::ssh::jump_control_command` with
/// `JumpControl::Check`) answers for, held by a pidfd ([`open_master`]), or
/// `None`. It asks twice, each within [`CONTROL_LIMIT`]: the id, then the
/// pidfd, then the id again, which must be the same. The master then held that
/// id at both answers, and the process the pidfd holds, opened between them,
/// is the master and no process that took its id afterwards. This is the way
/// to learn a master; nothing else composes these steps.
pub fn learn_master(check: &SshCommand) -> Option<OwnedFd> {
    let pid = master_pid(&run_control(check, CONTROL_LIMIT)?)?;
    let pidfd = open_master(pid)?;
    let again = master_pid(&run_control(check, CONTROL_LIMIT)?)?;
    (again == pid).then_some(pidfd)
}

/// The commands of one terminal pane's connection through a jump host, built
/// together ([`pane_commands`]): the GUI and the end-to-end check's helper run
/// exactly these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneCommands {
    /// Signs in to the jump host and serves the shared connection
    /// (`crate::ssh::jump_master_command`).
    pub master: SshCommand,
    /// The target's `ssh`, its `ProxyCommand` going through the master
    /// (`crate::ssh::terminal_command` with `crate::ssh::hop_command`).
    pub target: SshCommand,
    /// `-O check`, for [`learn_master`].
    pub check: SshCommand,
    /// `-O exit`, to end the master when its pid is not known.
    pub exit: SshCommand,
}

/// The commands of a pane's connection to `target`, running its startup
/// sequence `seq` if any, through `jump` at `socket`: the master with the jump
/// host's one-time token `jump_token`, the target with its own `target_token`
/// (each `None` when no password is stored for it, or the askpass helper is
/// missing), and the control commands. `None` when the hop refuses the jump
/// host's user name or host (`crate::ssh::hop_command`): the pane must then not
/// connect at all.
pub fn pane_commands(
    target: &Profile,
    seq: Option<&StartupSequence>,
    jump: &Profile,
    socket: &JumpSocket,
    askpass: &Path,
    target_token: Option<&AskpassToken>,
    jump_token: Option<&AskpassToken>,
) -> Option<PaneCommands> {
    let hop = hop_command(jump, socket.path())?;
    Some(PaneCommands {
        master: jump_master_command(jump, socket.path(), askpass, jump_token),
        target: terminal_command(target, seq, askpass, target_token, Some(&hop)),
        check: jump_control_command(jump, socket.path(), JumpControl::Check),
        exit: jump_control_command(jump, socket.path(), JumpControl::Exit),
    })
}

/// The commands of a sync's connection through a jump host, built together
/// ([`JumpLeg::for_sync`]) and run by `crate::sync::sync_over_ssh`: the master
/// signs in to the jump host, the sync's own `ssh` goes through it with
/// [`hop`](JumpLeg::hop) as its `ProxyCommand`, and the master is ended when
/// the sync returns.
///
/// One leg per sync, made from a new [`JumpSocket`]: its commands name that
/// socket, so a leg is not to be run twice (and cannot be copied).
#[derive(Debug, PartialEq, Eq)]
pub struct JumpLeg {
    /// The jump host's profile name, for what a failure says.
    pub name: String,
    /// Signs in to the jump host and serves the shared connection
    /// (`crate::ssh::jump_sync_master_command`).
    pub master: SshCommand,
    /// `-O check`, for [`learn_master`].
    pub check: SshCommand,
    /// `-O exit`, to end the master when its pid is not known.
    pub exit: SshCommand,
    /// The target's `ProxyCommand` (`crate::ssh::hop_command`), which
    /// `crate::ssh::sync_command` takes as its `proxy`.
    pub hop: String,
}

impl JumpLeg {
    /// The commands of a sync's connection through `jump` at `socket`, the
    /// master pointed at the askpass helper `askpass` when `has_password` (a
    /// sync's master has no one-time token, as a sync's `ssh` has none). `None`
    /// when the hop refuses the jump host's user name or host
    /// (`crate::ssh::hop_command`): the sync must then not start.
    pub fn for_sync(
        jump: &Profile,
        socket: &JumpSocket,
        askpass: &Path,
        has_password: bool,
    ) -> Option<JumpLeg> {
        let hop = hop_command(jump, socket.path())?;
        Some(JumpLeg {
            name: jump.name.clone(),
            master: jump_sync_master_command(jump, socket.path(), askpass, has_password),
            check: jump_control_command(jump, socket.path(), JumpControl::Check),
            exit: jump_control_command(jump, socket.path(), JumpControl::Exit),
            hop,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A live profile named `name` with the canonical id `n`.
    fn profile(n: u32, name: &str) -> Profile {
        let mut p = Profile::new(name, "example.com", "alice");
        p.id = format!("00000000-0000-4000-8000-{n:012x}");
        p
    }

    fn via(p: Profile, jump: &Profile) -> Profile {
        Profile {
            jump_host: Some(jump.id.clone()),
            ..p
        }
    }

    fn names<'a>(ps: &[&'a Profile]) -> Vec<&'a str> {
        ps.iter().map(|p| p.name.as_str()).collect()
    }

    #[test]
    fn routes() {
        let bastion = profile(1, "bastion");
        let db = via(profile(2, "db"), &bastion);
        let all = [bastion.clone(), db.clone()];
        let find = |id: &str| all.iter().find(|p| p.id == id);

        assert_eq!(route(&bastion, find), Route::Direct);
        assert_eq!(route(&db, find), Route::Through(&bastion));

        // A jump host nobody has.
        let lost = Profile {
            jump_host: Some(profile(9, "x").id),
            ..profile(3, "lost")
        };
        assert_eq!(route(&lost, find), Route::Missing);

        // A tombstone found by id is as good as none: `ProfileStore::get` never
        // gives one, but another caller's lookup may.
        let gone = [bastion.tombstone(5)];
        let find_gone = |id: &str| gone.iter().find(|p| p.id == id);
        assert_eq!(route(&db, find_gone), Route::Missing);

        // The jump host has a jump host of its own: one hop only.
        let cycle = [
            via(profile(4, "a"), &profile(5, "b")),
            via(profile(5, "b"), &profile(4, "a")),
        ];
        let find_cycle = |id: &str| cycle.iter().find(|p| p.id == id);
        assert_eq!(route(&cycle[0], find_cycle), Route::Chained(&cycle[1]));
        assert_eq!(route(&cycle[1], find_cycle), Route::Chained(&cycle[0]));
    }

    #[test]
    fn a_route_knows_its_jump_host_and_whether_it_refuses() {
        let bastion = profile(1, "bastion");
        let outer = via(profile(2, "outer"), &bastion);
        assert_eq!(Route::Direct.jump(), None);
        assert_eq!(Route::Missing.jump(), None);
        assert_eq!(Route::Through(&bastion).jump(), Some(&bastion));
        assert_eq!(Route::Chained(&outer).jump(), Some(&outer));
        assert_eq!(Route::Direct.refusal(), None);
        assert_eq!(Route::Through(&bastion).refusal(), None);
        assert!(Route::Missing.refusal().is_some());
        assert!(Route::Chained(&outer).refusal().is_some());

        // The jump host is borrowed from the profiles, not from the route, so
        // it can be kept after the route is gone.
        fn jump_of<'a>(p: &Profile, all: &'a [Profile]) -> Option<&'a Profile> {
            route(p, |id| all.iter().find(|q| q.id == id)).jump()
        }
        let db = via(profile(3, "db"), &bastion);
        let all = [bastion.clone(), db.clone()];
        assert_eq!(jump_of(&db, &all), Some(&bastion));
        assert_eq!(jump_of(&bastion, &all), None);
    }

    // ----- the socket -----

    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777
    }

    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    fn names_in(dir: &Path) -> Vec<std::ffi::OsString> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect()
    }

    /// A private temporary folder whose path is short and plain enough for a
    /// socket in it.
    fn runtime() -> tempfile::TempDir {
        let rt = tempfile::Builder::new()
            .prefix("rt")
            .tempdir_in("/tmp")
            .unwrap();
        set_mode(rt.path(), 0o700);
        rt
    }

    #[test]
    fn a_socket_lives_in_a_private_folder() {
        let rt = runtime();
        let socket = JumpSocket::new(rt.path()).unwrap();
        let dir = rt.path().join("moniker").join("jump");
        assert_eq!(socket.path().parent(), Some(dir.as_path()));
        assert_eq!(mode_of(&rt.path().join("moniker")), 0o700);
        assert_eq!(mode_of(&dir), 0o700);
        let name = socket.path().file_name().unwrap().to_str().unwrap();
        assert_eq!(name.len(), 32, "{name}");
        assert!(
            name.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "{name}"
        );
        // ssh makes the socket itself.
        assert!(names_in(&dir).is_empty());

        // The folder is there now and is used again; every socket has a name
        // of its own.
        let other = JumpSocket::new(rt.path()).unwrap();
        assert_ne!(other.path(), socket.path());
        assert_eq!(other.path().parent(), Some(dir.as_path()));
    }

    #[test]
    fn a_socket_refuses_a_folder_that_is_not_private() {
        // `moniker/jump` is a link to a private folder of this user's: only the
        // refusal to follow a link stops it.
        let elsewhere = runtime();
        let rt = runtime();
        std::fs::create_dir(rt.path().join("moniker")).unwrap();
        set_mode(&rt.path().join("moniker"), 0o700);
        std::os::unix::fs::symlink(elsewhere.path(), rt.path().join("moniker/jump")).unwrap();
        assert!(JumpSocket::new(rt.path()).is_err());
        assert!(names_in(elsewhere.path()).is_empty());

        // `moniker/jump` open to others, and `moniker` itself.
        for (moniker, jump) in [(0o700, 0o755), (0o755, 0o700), (0o700, 0o701)] {
            let rt = runtime();
            JumpSocket::new(rt.path()).unwrap();
            set_mode(&rt.path().join("moniker"), moniker);
            set_mode(&rt.path().join("moniker/jump"), jump);
            assert!(JumpSocket::new(rt.path()).is_err(), "{moniker:o}/{jump:o}");
        }

        // A file where the folder belongs.
        let rt = runtime();
        std::fs::create_dir(rt.path().join("moniker")).unwrap();
        set_mode(&rt.path().join("moniker"), 0o700);
        std::fs::write(rt.path().join("moniker/jump"), "").unwrap();
        assert!(JumpSocket::new(rt.path()).is_err());
    }

    #[test]
    fn a_socket_refuses_an_unsafe_runtime_folder() {
        // The socket's path goes into a shell string (the hop's ProxyCommand),
        // so only `[A-Za-z0-9/._-]` may be in it, and it must leave ssh room
        // for the temporary name it adds (`sun_path` is 108 bytes).
        let rt = runtime();
        for name in [
            "a b",
            "it's",
            "50%",
            "tab\there",
            "dollar$x",
            "back`tick",
            "é",
        ] {
            let dir = rt.path().join(name);
            std::fs::create_dir(&dir).unwrap();
            set_mode(&dir, 0o700);
            let err = JumpSocket::new(&dir).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{name:?}");
            assert!(names_in(&dir).is_empty(), "nothing made in {name:?}");
        }
        let err = JumpSocket::new(Path::new("relative/run")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(!Path::new("relative").exists());

        // `<runtime>/moniker/jump/` and 32 characters: 46 bytes after the
        // runtime folder. 90 bytes in all pass, 91 do not.
        let base = rt.path().as_os_str().len();
        assert!(base < 40, "{}", rt.path().display());
        let fits = rt.path().join("f".repeat(MAX_SOCKET_PATH - 46 - base - 1));
        let over = rt.path().join("o".repeat(MAX_SOCKET_PATH - 46 - base));
        for dir in [&fits, &over] {
            std::fs::create_dir(dir).unwrap();
            set_mode(dir, 0o700);
        }
        let socket = JumpSocket::new(&fits).unwrap();
        assert_eq!(socket.path().as_os_str().len(), MAX_SOCKET_PATH);
        assert_eq!(MAX_SOCKET_PATH, 90);
        let err = JumpSocket::new(&over).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(names_in(&over).is_empty());
    }

    #[test]
    fn a_socket_refuses_a_runtime_folder_others_can_change() {
        // Writable by the group or by others: someone else could move
        // `moniker` aside and put a folder of their own in its place between
        // the check and ssh's use of the path.
        for mode in [0o770, 0o730, 0o707, 0o702, 0o777, 0o1777] {
            let rt = runtime();
            set_mode(rt.path(), mode);
            let err = JumpSocket::new(rt.path()).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{mode:o}");
            assert!(names_in(rt.path()).is_empty(), "nothing made at {mode:o}");
        }
        // Readable by others is no matter.
        for mode in [0o700, 0o750, 0o755] {
            let rt = runtime();
            set_mode(rt.path(), mode);
            assert!(JumpSocket::new(rt.path()).is_ok(), "{mode:o}");
        }
        // Not this user's: `/proc` is root's. The refusal is this check's own,
        // not the failure to make a folder there (as root, mkdir there fails
        // anyway).
        if !rustix::process::geteuid().is_root() {
            let err = JumpSocket::new(Path::new("/proc")).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
            assert!(err.to_string().contains("not this user's"), "{err}");
        }
        assert!(!Path::new("/proc/moniker").exists());
    }

    #[test]
    fn a_socket_is_never_a_path_that_exists() {
        // A master started on a path that is taken runs on as no master at
        // all, which neither ControlPersist nor `-O` could ever end.
        const NAME: &str = "0123456789abcdef0123456789abcdef";
        let rt = runtime();
        let socket = JumpSocket::named(rt.path(), NAME).unwrap();
        let path = socket.path().to_path_buf();
        assert_eq!(path, rt.path().join("moniker/jump").join(NAME));
        std::fs::write(&path, "").unwrap();
        let err = JumpSocket::named(rt.path(), NAME).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        // A link, even one to nothing, is a path that is taken too.
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(rt.path().join("nowhere"), &path).unwrap();
        let err = JumpSocket::named(rt.path(), NAME).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        std::fs::remove_file(&path).unwrap();
        assert!(JumpSocket::named(rt.path(), NAME).is_ok());
    }

    // ----- the master -----

    #[test]
    fn master_pid_reads_ssh_s_answer() {
        // As OpenSSH 10.2 writes it on stderr, with CR LF.
        assert_eq!(master_pid("Master running (pid=4242)\r\n"), Some(4242));
        assert_eq!(master_pid("Master running (pid=4242)\n"), Some(4242));
        assert_eq!(master_pid("Master running (pid=4242)"), Some(4242));
        // After whatever ssh printed first (a LogLevel set in the config).
        assert_eq!(
            master_pid(
                "debug1: Reading configuration data /etc/ssh/ssh_config\r\nMaster running (pid=77)\r\n"
            ),
            Some(77)
        );
        // ssh writes its answer last: a line before it (a `Match exec`
        // command's own output, say) is not the master's.
        assert_eq!(
            master_pid("Master running (pid=11)\r\nMaster running (pid=22)\r\n"),
            Some(22)
        );
        assert_eq!(
            master_pid("Master running (pid=11)\nsomething else\r\nMaster running (pid=22)\r\n"),
            Some(22)
        );
        // The last such line decides: one that does not read leaves no id,
        // never an earlier line's.
        assert_eq!(
            master_pid("Master running (pid=11)\r\nMaster running (pid=x)\r\n"),
            None
        );
        for text in [
            "",
            "Master running (pid=)\r\n",
            "Master running (pid=99999999999)\r\n",
            "Master running (pid=0)\r\n",
            "Master running (pid=-1)\r\n",
            "Master running (pid=+5)\r\n",
            "Master running (pid= 5)\r\n",
            "Master running (pid=5) \r\n",
            "Master running (pid=5\r\n",
            "Master running pid=5\r\n",
            "Control socket connect(/run/x): No such file or directory\r\n",
            "Exit request sent.\r\n",
        ] {
            assert_eq!(master_pid(text), None, "{text:?}");
        }
    }

    fn sh(script: &str) -> SshCommand {
        SshCommand {
            program: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            env: vec![("FROM_THE_COMMAND".to_string(), "yes".to_string())],
        }
    }

    #[test]
    fn run_control_returns_only_a_clean_exit() {
        let limit = Duration::from_secs(5);
        // Stderr is what counts; stdout goes nowhere; the command's own
        // variables are added.
        let out = run_control(
            &sh("echo out; printf 'Master running (pid=12)\\r\\n%s\\n' \"$FROM_THE_COMMAND\" >&2"),
            limit,
        );
        assert_eq!(out.as_deref(), Some("Master running (pid=12)\r\nyes\n"));
        // It gets no input: a read sees the end at once.
        assert_eq!(
            run_control(&sh("read x; echo \"[$x]\" >&2"), limit).as_deref(),
            Some("[]\n")
        );
        // Any other end is nothing.
        assert_eq!(
            run_control(&sh("echo 'Master running (pid=12)' >&2; exit 255"), limit),
            None
        );
        assert_eq!(run_control(&sh("kill -TERM $$"), limit), None);
        assert_eq!(
            run_control(
                &SshCommand {
                    program: "/nonexistent/ssh".to_string(),
                    args: Vec::new(),
                    env: Vec::new(),
                },
                limit
            ),
            None
        );

        // Too slow: killed and reaped at the limit.
        let start = Instant::now();
        assert_eq!(
            run_control(&sh("sleep 5"), Duration::from_millis(200)),
            None
        );
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn run_control_drains_a_long_stderr_and_keeps_its_end() {
        // Far more than a pipe holds: the command must not block on it.
        let out = run_control(
            &sh("head -c 200000 /dev/zero | tr '\\0' x >&2; printf '\\nMaster running (pid=12)\\r\\n' >&2"),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(out.len(), CONTROL_MAX_BYTES);
        assert!(
            out.ends_with("\nMaster running (pid=12)\r\n"),
            "{:?}",
            &out[out.len() - 40..]
        );
        assert_eq!(master_pid(&out), Some(12));
    }

    #[test]
    fn a_master_is_ended_through_its_pidfd() {
        use std::os::unix::process::ExitStatusExt;
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pidfd = open_master(child.id()).unwrap();
        end_master(&pidfd).unwrap();
        assert_eq!(child.wait().unwrap().signal(), Some(15));
        // Once it is gone (and reaped), the pidfd cannot reach anything else.
        assert!(end_master(&pidfd).is_err());

        // Never this process, init, or no process at all.
        for pid in [0, 1, std::process::id(), u32::MAX, i32::MAX as u32 + 1] {
            assert!(open_master(pid).is_none(), "{pid}");
        }
    }

    /// A process that stands for a master, ended when the test is over.
    struct Stand(std::process::Child);

    impl Stand {
        fn new() -> Stand {
            Stand(
                std::process::Command::new("sleep")
                    .arg("30")
                    .spawn()
                    .unwrap(),
            )
        }

        fn pid(&self) -> u32 {
            self.0.id()
        }

        /// The signal that ended it, once it has ended.
        fn ended_by(&mut self) -> Option<i32> {
            use std::os::unix::process::ExitStatusExt;
            self.0
                .try_wait()
                .unwrap()
                .and_then(|status| status.signal())
        }
    }

    impl Drop for Stand {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// A `-O check` that answers with `first` the first time it runs and with
    /// `then` after that (a file in `dir` tells), or fails with `then: None`.
    fn check_answering(dir: &Path, first: u32, then: Option<u32>) -> SshCommand {
        let asked = dir.join("asked");
        let second = match then {
            Some(pid) => format!("printf 'Master running (pid={pid})\\r\\n' >&2"),
            None => "exit 255".to_string(),
        };
        sh(&format!(
            "if [ -e '{asked}' ]; then {second}; else : >'{asked}'; \
             printf 'Master running (pid={first})\\r\\n' >&2; fi",
            asked = asked.display(),
        ))
    }

    #[test]
    fn learn_master_holds_the_pid_both_checks_named() {
        let dir = tempfile::tempdir().unwrap();

        // The same pid twice: the pidfd is that process's.
        let mut master = Stand::new();
        let check = check_answering(dir.path(), master.pid(), Some(master.pid()));
        let pidfd = learn_master(&check).unwrap();
        assert_eq!(master.ended_by(), None, "learning ends nothing");
        end_master(&pidfd).unwrap();
        let start = Instant::now();
        while master.ended_by().is_none() && start.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(master.ended_by(), Some(15));

        // Another pid the second time: the first may no longer be the
        // master's, so nothing is held, and neither process is touched.
        let dir = tempfile::tempdir().unwrap();
        let (mut one, mut other) = (Stand::new(), Stand::new());
        let check = check_answering(dir.path(), one.pid(), Some(other.pid()));
        assert!(learn_master(&check).is_none());
        assert_eq!((one.ended_by(), other.ended_by()), (None, None));

        // The second check fails, or the first one does.
        let dir = tempfile::tempdir().unwrap();
        let check = check_answering(dir.path(), one.pid(), None);
        assert!(learn_master(&check).is_none());
        assert!(learn_master(&sh("echo 'Master running (pid=1)' >&2; exit 255")).is_none());
        // A pid that is no process, or this one's.
        let me = std::process::id();
        assert!(learn_master(&sh(&format!("echo 'Master running (pid={me})' >&2"))).is_none());
        assert_eq!(one.ended_by(), None);
    }

    #[test]
    fn a_pane_s_commands_are_the_builders_together() {
        let rt = runtime();
        let socket = JumpSocket::new(rt.path()).unwrap();
        let askpass = Path::new("/usr/lib/moniker/moniker-askpass");
        let jump = profile(1, "bastion");
        let target = via(profile(2, "db"), &jump);
        let seq = StartupSequence {
            name: "u".to_string(),
            commands: vec!["uptime".to_string()],
        };
        let (t, j) = (AskpassToken::new(), AskpassToken::new());
        let cmds = pane_commands(
            &target,
            Some(&seq),
            &jump,
            &socket,
            askpass,
            Some(&t),
            Some(&j),
        )
        .unwrap();
        let hop = hop_command(&jump, socket.path()).unwrap();
        assert_eq!(
            cmds,
            PaneCommands {
                master: jump_master_command(&jump, socket.path(), askpass, Some(&j)),
                target: terminal_command(&target, Some(&seq), askpass, Some(&t), Some(&hop)),
                check: jump_control_command(&jump, socket.path(), JumpControl::Check),
                exit: jump_control_command(&jump, socket.path(), JumpControl::Exit),
            }
        );
        // The master carries the jump host's token, the target its own.
        let token_of = |cmd: &SshCommand| {
            cmd.env
                .iter()
                .find(|(name, _)| name == "MONIKER_ASKPASS_TOKEN")
                .map(|(_, value)| value.clone())
        };
        assert_eq!(token_of(&cmds.master).as_deref(), Some(j.as_str()));
        assert_eq!(token_of(&cmds.target).as_deref(), Some(t.as_str()));

        // A jump host whose user or host a shell would read: no commands.
        let mut odd = jump.clone();
        odd.host = "bastion;id".to_string();
        assert_eq!(
            pane_commands(&target, None, &odd, &socket, askpass, None, None),
            None
        );
    }

    #[test]
    fn a_sync_s_jump_leg_is_the_builders_together() {
        let rt = runtime();
        let socket = JumpSocket::new(rt.path()).unwrap();
        let askpass = Path::new("/usr/lib/moniker/moniker-askpass");
        let jump = profile(1, "bastion");
        for has_password in [true, false] {
            let leg = JumpLeg::for_sync(&jump, &socket, askpass, has_password).unwrap();
            assert_eq!(
                leg,
                JumpLeg {
                    name: "bastion".to_string(),
                    master: jump_sync_master_command(&jump, socket.path(), askpass, has_password),
                    check: jump_control_command(&jump, socket.path(), JumpControl::Check),
                    exit: jump_control_command(&jump, socket.path(), JumpControl::Exit),
                    hop: hop_command(&jump, socket.path()).unwrap(),
                }
            );
            // A sync has no terminal to fall back to: no one-time token.
            assert!(
                leg.master
                    .env
                    .iter()
                    .all(|(name, _)| name != "MONIKER_ASKPASS_TOKEN"),
                "{:?}",
                leg.master.env
            );
        }

        // A jump host whose user or host a shell would read: no leg.
        let mut odd = jump.clone();
        odd.username = "alice$(id)".to_string();
        assert_eq!(JumpLeg::for_sync(&odd, &socket, askpass, true), None);
    }

    #[test]
    fn refusals_name_the_jump_host() {
        assert_eq!(MISSING_TEXT, "This profile's jump host is no longer saved.");
        assert_eq!(Route::Missing.refusal().as_deref(), Some(MISSING_TEXT));
        assert_eq!(
            chain_text("bastion"),
            "bastion goes through a jump host of its own. Moniker uses only one jump host."
        );
        // The name is the jump host's, the profile that has a jump host of its own.
        let inner = via(profile(1, "inner"), &profile(2, "outer"));
        assert_eq!(Route::Chained(&inner).refusal(), Some(chain_text("inner")));
    }

    #[test]
    fn going_through_lists_live_profiles_in_name_order() {
        let bastion = profile(1, "bastion");
        let all = vec![
            via(profile(2, "web"), &bastion),
            via(profile(3, "Db"), &bastion),
            profile(4, "direct"),
            via(profile(5, "app"), &bastion),
            via(profile(6, "other"), &profile(7, "somebody else")),
            via(profile(8, "gone"), &bastion).tombstone(3),
            // The same name once more: the id decides.
            via(profile(9, "web"), &bastion),
            bastion.clone(),
        ];
        let through = going_through(&bastion.id, &all);
        assert_eq!(names(&through), ["app", "Db", "web", "web"]);
        assert!(through[2].id < through[3].id);
        assert!(going_through(&all[4].id, &all).is_empty());
        assert!(going_through("not an id", &all).is_empty());
    }

    #[test]
    fn usable_jump_hosts_leave_out_self_chains_and_tombstones() {
        let bastion = profile(1, "Bastion");
        let edge = profile(2, "edge");
        let behind = via(profile(3, "behind"), &bastion);
        let gone = profile(4, "gone").tombstone(3);
        let me = profile(5, "me");
        let all = vec![me.clone(), behind, gone, edge, bastion];
        let usable = usable_jump_hosts(&me.id, &all);
        assert_eq!(names(&usable), ["Bastion", "edge"]);
        // A profile that has a jump host of its own can be nobody's.
        assert!(usable.iter().all(|p| p.jump_host.is_none()));
        // For a profile that is not in the list, only the tombstone and the
        // profile with a jump host are left out.
        let usable = usable_jump_hosts("someone else", &all);
        assert_eq!(names(&usable), ["Bastion", "edge", "me"]);
    }

    #[test]
    fn holds_jump_host_looks_at_live_profiles() {
        let bastion = profile(1, "bastion");
        let db = via(profile(2, "db"), &bastion);
        assert!(!holds_jump_host(std::slice::from_ref(&bastion)));
        assert!(holds_jump_host(&[bastion.clone(), db.clone()]));
        assert!(!holds_jump_host(&[bastion, db.tombstone(4)]));
        assert!(!holds_jump_host(&[]));
    }
}
