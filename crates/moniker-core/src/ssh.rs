//! Building the `ssh` command line for a [`Profile`].
//!
//! Pure functions: nothing here spawns a process or touches the keyring. The
//! GUI spawns [`terminal_command`] inside a VTE tab and the sync code spawns
//! [`sync_command`] headlessly.
//!
//! A profile with a jump host is reached in two steps, each an `ssh` that
//! Moniker starts itself, so that the askpass helper answers each one's own
//! login (a hop's `ssh` is never Moniker's child). First
//! [`jump_master_command`] (or [`jump_sync_master_command`] for a sync) signs in
//! to the jump host as the master of a shared connection at a private socket
//! (`crate::jump::JumpSocket`) and goes to the background. Then the target's
//! command runs with a `ProxyCommand` ([`hop_command`]) whose `ssh` only asks
//! that master for a channel to the target (`-W`). [`jump_control_command`]
//! asks the master for its process id or tells it to end. No TCP port is
//! opened. The master ends by itself `crate::jump::PERSIST_SECS` after its last
//! channel closed, should Moniker not end it first.
//!
//! The builders take no password, so none can reach argv or the environment
//! (both are visible through `/proc`). When a password is stored, `ssh` is
//! pointed at the `moniker-askpass` helper through `SSH_ASKPASS`; only the
//! non-secret profile id, the id of the Moniker process that starts `ssh`
//! (and, for a terminal tab, the non-secret one-time token, see
//! `crate::askpass`) travel in the environment, and the helper looks the
//! password up in the keyring itself.
//!
//! The builders assume a validated live profile (see `Profile::validate`).
//! The `--` separator before the destination is a second line of defense in
//! case a bad host or username ever slips through: after `--`, `ssh` cannot
//! parse the destination as an option.
//!
//! Paths (`key_file`, the askpass helper) are converted with
//! [`Path::to_string_lossy`]. A path that is not valid UTF-8 is unsupported:
//! the replacement characters would name a different file.

use std::path::Path;

use crate::askpass::{AskpassToken, PID_ENV, TOKEN_ENV};
use crate::jump::PERSIST_SECS;
use crate::profile::{Profile, StartupSequence};

/// The system `ssh` binary, found on `PATH` so ssh-agent, `known_hosts` and
/// `~/.ssh/config` keep working.
const SSH_PROGRAM: &str = "ssh";

/// The program a sync runs on the remote machine, found on the remote's
/// non-interactive `PATH`. Callers pass it to [`sync_command`].
pub const SERVER_PROGRAM: &str = "moniker-server";

/// A command ready to spawn: the program, its arguments and the environment
/// variables to add. The environment is additive; the caller inherits the rest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshCommand {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// The remote command that runs a startup sequence and then leaves the user in
/// an interactive login shell, or `None` when there is nothing to run.
///
/// Commands are joined with a newline, not `;`: a command ending in a
/// `# comment` would swallow a following `exec`, and one ending in `&` would
/// make `&;` a shell syntax error. Commands cannot contain newlines (profile
/// validation), so the newline is safe. Blank commands are skipped; if none
/// remain, or `seq` is `None`, the result is `None`. The remaining commands are
/// passed through verbatim: they are the user's own shell commands.
pub fn remote_command(seq: Option<&StartupSequence>) -> Option<String> {
    Some(format!("{}\nexec \"$SHELL\" -l", commands(seq?)?))
}

/// The script a local terminal runs for a startup sequence, or `None` when
/// there is nothing to run. It is [`remote_command`] for this machine: the
/// caller runs it with the person's own shell as `shell -c <script>`, with
/// `SHELL` set to that shell, and it ends in `exec "$SHELL"`.
///
/// The commands are joined and skipped exactly as [`remote_command`] does, and
/// an empty line comes before the `exec`: a command ending in a backslash (a
/// line continuation) would otherwise take the `exec` line as part of itself,
/// and the sequence would leave no shell. The empty line ends that command
/// first. (A backslash at the end of an earlier command still joins the next
/// command to it: those lines are the person's own.)
///
/// There is no `-l`: a plain local terminal's shell is not a login shell, so
/// the shell this leaves is the same kind. The shell's path travels in the
/// environment (`$SHELL`), never in the script, so a path with a space or a
/// quote cannot change what runs.
pub fn local_command(seq: Option<&StartupSequence>) -> Option<String> {
    Some(format!("{}\n\nexec \"$SHELL\"", commands(seq?)?))
}

/// The non-blank commands of `seq`, one to a line; `None` when no command is
/// left. The joining is the one [`remote_command`] documents.
fn commands(seq: &StartupSequence) -> Option<String> {
    let commands: Vec<&str> = seq
        .commands
        .iter()
        .map(String::as_str)
        .filter(|c| !c.trim().is_empty())
        .collect();
    if commands.is_empty() {
        return None;
    }
    Some(commands.join("\n"))
}

/// `ssh` invocation for an interactive terminal tab.
///
/// Arguments, in order: `-p <port>`, `-i <key>` if the profile has a key file,
/// `-o ProxyCommand=<proxy>` if the connection goes through a jump host
/// (`proxy` is [`hop_command`]'s, one argv entry), `-t` only if there is a
/// remote command (so the startup sequence gets a tty), `--`,
/// `<username>@<host>`, then the remote command as a single argument if any.
///
/// A `ProxyCommand` on the command line comes before anything in
/// `~/.ssh/config`, so a `ProxyJump` or `ProxyCommand` the configuration sets
/// for this host is not used: the profile's route wins. It changes no `user`,
/// `hostname` or `hostkeyalias` (no `Match` criterion reads it), so the
/// identity the askpass helper expects ([`resolve_command`]) is the same.
///
/// There is no limit on password prompts: `ssh` allows its usual three. The
/// askpass helper answers at most one of them with the stored password (the
/// one-time marker named by `token`, see `crate::askpass`); after a wrong one
/// the user types the password in the terminal, as with plain `ssh`.
///
/// With a `token` (a password is stored) the environment points `ssh` at the
/// askpass helper: exactly `SSH_ASKPASS=<askpass>`, `SSH_ASKPASS_REQUIRE=force`,
/// `MONIKER_PROFILE_ID=<profile id>`, `MONIKER_PID=<this process's id>` and
/// `MONIKER_ASKPASS_TOKEN=<token>`, in that order. Without one it is empty.
/// None of them is secret. `MONIKER_PID` is the process that spawns `ssh`
/// (VTE forks the child from it), which the helper checks is the parent of the
/// `ssh` that prompts (`crate::askpass::started_by_moniker`).
pub fn terminal_command(
    p: &Profile,
    seq: Option<&StartupSequence>,
    askpass: &Path,
    token: Option<&AskpassToken>,
    proxy: Option<&str>,
) -> SshCommand {
    debug_assert!(!p.deleted, "ssh command requested for a tombstone");
    let remote = remote_command(seq);
    let mut args = connection_args(p);
    push_proxy(&mut args, proxy);
    if remote.is_some() {
        args.push("-t".to_string());
    }
    push_destination(&mut args, p);
    args.extend(remote);
    SshCommand {
        program: SSH_PROGRAM.to_string(),
        args,
        env: token_env(p, askpass, token),
    }
}

/// `ssh` invocation that runs `<remote_server> sync-serve` on the profile's
/// host with no tty.
///
/// Arguments, in order: `-p <port>`, `-i <key>` if the profile has a key file,
/// `-o ProxyCommand=<proxy>` if the connection goes through a jump host (as for
/// [`terminal_command`]), `-T`, `-o StrictHostKeyChecking=yes`,
/// `-o NumberOfPasswordPrompts=1`, `--`, `<username>@<host>`,
/// `<remote_server>`, `sync-serve`. With no tty, an unknown host key fails
/// instead of prompting.
///
/// `remote_server` is the remote command name or path, normally
/// [`SERVER_PROGRAM`]. It is trusted, code-supplied input: it is not quoted and
/// the remote shell splits it, so it must never come from a profile or the
/// network.
///
/// With `has_password` the environment points `ssh` at the askpass helper:
/// exactly `SSH_ASKPASS=<askpass>`, `SSH_ASKPASS_REQUIRE=force`,
/// `MONIKER_PROFILE_ID=<profile id>` and `MONIKER_PID=<this process's id>` (the
/// caller must spawn `ssh` itself), in that order, and no one-time token: a
/// sync has no terminal to fall back to, so `NumberOfPasswordPrompts=1` is what
/// keeps a wrong stored password from being sent twice. Otherwise it is empty.
pub fn sync_command(
    p: &Profile,
    askpass: &Path,
    has_password: bool,
    remote_server: &str,
    proxy: Option<&str>,
) -> SshCommand {
    debug_assert!(!p.deleted, "ssh command requested for a tombstone");
    let mut args = connection_args(p);
    push_proxy(&mut args, proxy);
    args.push("-T".to_string());
    push_option(&mut args, "StrictHostKeyChecking=yes");
    push_option(&mut args, "NumberOfPasswordPrompts=1");
    push_destination(&mut args, p);
    args.push(remote_server.to_string());
    args.push("sync-serve".to_string());
    SshCommand {
        program: SSH_PROGRAM.to_string(),
        args,
        env: askpass_env(p, askpass, has_password),
    }
}

/// The `ProxyCommand` that takes a connection to its target through the jump
/// host's shared connection at `socket`: exactly
/// `ssh -S <socket> -o ControlMaster=no -o BatchMode=yes -W %h:%p -p <port> -- <username>@<host>`
/// with every `%` of the host doubled, given to [`terminal_command`] or
/// [`sync_command`] as their `proxy`.
///
/// It is the one shell string Moniker makes: `ssh` expands its `%` tokens and
/// runs it with `$SHELL -c`. So nothing in it is quoted text: the socket's path
/// is `[A-Za-z0-9/._-]` (`crate::jump::JumpSocket` makes no other), the user
/// name `[A-Za-z0-9._@-]` and the host `[A-Za-z0-9._:%-]`
/// (`Profile::validate`), none of them special to a shell, and the port is
/// digits. A host's `%` (an IPv6 zone, `fe80::1%eth0`) is doubled so that the
/// expansion gives it back instead of reading a token (`%e` is none, and ssh
/// would refuse the command).
///
/// `-W %h:%p` asks the master for a channel to the target's host and port as
/// the target's `ssh` sees them (after `~/.ssh/config`). `ControlMaster=no`
/// keeps this `ssh` from becoming a master itself. `BatchMode=yes` and no `-i`:
/// should the shared connection be gone, this `ssh`, a child of the target's
/// and not of Moniker, would have to sign in to the jump host itself; it then
/// fails (or uses ssh-agent) and never asks for anything, and no key-file path
/// enters the shell string.
pub fn hop_command(jump: &Profile, socket: &Path) -> String {
    debug_assert!(!jump.deleted, "ssh command requested for a tombstone");
    let socket = socket.to_string_lossy();
    debug_assert!(
        [&*socket, &jump.username, &jump.host]
            .iter()
            .all(|word| word.bytes().all(is_hop_byte)),
        "a value of the hop's shell string has an unsafe character"
    );
    format!(
        "{SSH_PROGRAM} -S {socket} -o ControlMaster=no -o BatchMode=yes -W %h:%p -p {port} \
         -- {user}@{host}",
        port = jump.port,
        user = jump.username,
        host = jump.host.replace('%', "%%"),
    )
}

/// Whether `byte` may be in a value of [`hop_command`]'s shell string: the
/// characters of the socket's path, the user name and the host, together.
fn is_hop_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-' | b':' | b'%' | b'@')
}

/// `ssh` invocation that signs in to the jump host `jump` in a terminal tab
/// and then serves as the master of a shared connection at `socket`, in the
/// background.
///
/// Arguments, in order: `-p <port>`, `-i <key>` if the jump host's profile has
/// a key file, `-f`, `-N`, `-M`, `-S <socket>`,
/// `-o ControlPersist=<PERSIST_SECS>`, `--`, `<username>@<host>`. `-f` sends
/// `ssh` to the background once it has signed in, so the tab's terminal sees it
/// exit (status 0) and can run the target's `ssh` next; `-N` runs nothing on
/// the jump host. The master ends by itself `PERSIST_SECS` after its last
/// channel closed: a backstop, should Moniker not end it first.
///
/// Nothing here sets a route for the jump host: a `ProxyJump` that
/// `~/.ssh/config` sets for it is followed (and its hop asks in the terminal,
/// as for any connection).
///
/// The environment is [`terminal_command`]'s for `token`, with the jump host's
/// id: the five variables when its password is stored, else empty. The `ssh`
/// that asks for the password is this one, Moniker's child, before it goes to
/// the background.
pub fn jump_master_command(
    jump: &Profile,
    socket: &Path,
    askpass: &Path,
    token: Option<&AskpassToken>,
) -> SshCommand {
    SshCommand {
        program: SSH_PROGRAM.to_string(),
        args: master_args(jump, socket, &[]),
        env: token_env(jump, askpass, token),
    }
}

/// [`jump_master_command`] for a sync, which has no tty and no terminal to ask
/// in.
///
/// Arguments, in order: those of [`jump_master_command`] up to
/// `-o ControlPersist=<PERSIST_SECS>`, then `-o StrictHostKeyChecking=yes`,
/// `-o NumberOfPasswordPrompts=1`, `--`, `<username>@<host>`. The environment
/// is [`sync_command`]'s for the jump host: with `has_password`, the four
/// variables with its id and no one-time token, else empty.
pub fn jump_sync_master_command(
    jump: &Profile,
    socket: &Path,
    askpass: &Path,
    has_password: bool,
) -> SshCommand {
    SshCommand {
        program: SSH_PROGRAM.to_string(),
        args: master_args(
            jump,
            socket,
            &["StrictHostKeyChecking=yes", "NumberOfPasswordPrompts=1"],
        ),
        env: askpass_env(jump, askpass, has_password),
    }
}

/// The master's arguments ([`jump_master_command`]), with `options` as `-o`
/// options after `ControlPersist`.
fn master_args(jump: &Profile, socket: &Path, options: &[&str]) -> Vec<String> {
    debug_assert!(!jump.deleted, "ssh command requested for a tombstone");
    let mut args = connection_args(jump);
    args.extend(["-f", "-N", "-M", "-S"].map(String::from));
    args.push(socket.to_string_lossy().into_owned());
    push_option(&mut args, &format!("ControlPersist={PERSIST_SECS}"));
    for option in options {
        push_option(&mut args, option);
    }
    push_destination(&mut args, jump);
    args
}

/// What [`jump_control_command`] asks the master.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JumpControl {
    /// Whether it runs, and its process id: `Master running (pid=N)` on stderr
    /// (`crate::jump::master_pid`).
    Check,
    /// To end at once: it closes its channels and removes the socket.
    Exit,
}

/// `ssh -O check` or `ssh -O exit` for the master at `socket`, which serves the
/// jump host `jump`.
///
/// Arguments, in order: `-S <socket>`, `-O`, `check` or `exit`, `-p <port>`,
/// `--`, `<username>@<host>`. No environment: it only talks to the master over
/// the socket, and when no master answers there it fails at once instead of
/// connecting, so it never signs in or asks for anything.
pub fn jump_control_command(jump: &Profile, socket: &Path, op: JumpControl) -> SshCommand {
    debug_assert!(!jump.deleted, "ssh command requested for a tombstone");
    let op = match op {
        JumpControl::Check => "check",
        JumpControl::Exit => "exit",
    };
    let mut args = vec![
        "-S".to_string(),
        socket.to_string_lossy().into_owned(),
        "-O".to_string(),
        op.to_string(),
        "-p".to_string(),
        jump.port.to_string(),
    ];
    push_destination(&mut args, jump);
    SshCommand {
        program: SSH_PROGRAM.to_string(),
        args,
        env: Vec::new(),
    }
}

/// `ssh -G` for the connection [`terminal_command`] and [`sync_command`] make
/// to the profile: `ssh` prints the configuration it would use, after
/// `~/.ssh/config`, and exits without connecting. The askpass helper reads from
/// it the user and host that the password prompt will name
/// (`crate::askpass::effective_identity`).
///
/// Arguments, in order: `-G`, `-p <port>`, `-i <key>` if the profile has a key
/// file, `--`, `<username>@<host>`. No environment.
pub fn resolve_command(p: &Profile) -> SshCommand {
    debug_assert!(!p.deleted, "ssh command requested for a tombstone");
    let mut args = vec!["-G".to_string()];
    args.extend(connection_args(p));
    push_destination(&mut args, p);
    SshCommand {
        program: SSH_PROGRAM.to_string(),
        args,
        env: Vec::new(),
    }
}

/// `-p <port>` and, if the profile has one, `-i <key>`. Each flag and its value
/// are separate argv entries.
fn connection_args(p: &Profile) -> Vec<String> {
    let mut args = vec!["-p".to_string(), p.port.to_string()];
    if let Some(key) = &p.key_file {
        args.push("-i".to_string());
        args.push(key.to_string_lossy().into_owned());
    }
    args
}

/// `-o <option>` as two argv entries.
fn push_option(args: &mut Vec<String>, option: &str) {
    args.push("-o".to_string());
    args.push(option.to_string());
}

/// `-o ProxyCommand=<proxy>`, if there is a `proxy`.
fn push_proxy(args: &mut Vec<String>, proxy: Option<&str>) {
    if let Some(proxy) = proxy {
        push_option(args, &format!("ProxyCommand={proxy}"));
    }
}

/// The `--` separator and then `<username>@<host>`, so that neither can be
/// parsed as an `ssh` option.
fn push_destination(args: &mut Vec<String>, p: &Profile) {
    args.push("--".to_string());
    args.push(format!("{}@{}", p.username, p.host));
}

/// The environment that makes `ssh` ask `moniker-askpass` for the password:
/// empty when no password is stored. Carries only the non-secret profile id
/// and this process's id; [`token_env`] adds a terminal connection's token.
fn askpass_env(p: &Profile, askpass: &Path, has_password: bool) -> Vec<(String, String)> {
    if !has_password {
        return Vec::new();
    }
    vec![
        (
            "SSH_ASKPASS".to_string(),
            askpass.to_string_lossy().into_owned(),
        ),
        ("SSH_ASKPASS_REQUIRE".to_string(), "force".to_string()),
        ("MONIKER_PROFILE_ID".to_string(), p.id.clone()),
        (PID_ENV.to_string(), std::process::id().to_string()),
    ]
}

/// [`askpass_env`] and the connection's one-time token, for an `ssh` in a
/// terminal tab ([`terminal_command`], [`jump_master_command`]); empty without
/// a token (no password stored, or no helper to answer).
fn token_env(p: &Profile, askpass: &Path, token: Option<&AskpassToken>) -> Vec<(String, String)> {
    let Some(token) = token else {
        return Vec::new();
    };
    let mut env = askpass_env(p, askpass, true);
    env.push((TOKEN_ENV.to_string(), token.as_str().to_string()));
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    fn askpass() -> &'static Path {
        Path::new("/usr/lib/moniker/moniker-askpass")
    }

    fn profile() -> Profile {
        Profile::new("Prod web", "example.com", "alice")
    }

    fn seq(commands: &[&str]) -> StartupSequence {
        StartupSequence {
            name: "update".to_string(),
            commands: commands.iter().map(|c| c.to_string()).collect(),
        }
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn position(args: &[String], needle: &str) -> usize {
        args.iter()
            .position(|a| a == needle)
            .unwrap_or_else(|| panic!("{needle:?} not found in {args:?}"))
    }

    #[test]
    fn no_sequence_means_no_remote_command() {
        assert_eq!(remote_command(None), None);
        let cmd = terminal_command(&profile(), None, askpass(), None, None);
        assert!(!cmd.args.contains(&"-t".to_string()), "{:?}", cmd.args);
        assert_eq!(cmd.args.last().unwrap(), "alice@example.com");
    }

    #[test]
    fn sequence_joins_then_execs_shell() {
        let s = seq(&["sudo apt update", "uptime"]);
        assert_eq!(
            remote_command(Some(&s)),
            Some("sudo apt update\nuptime\nexec \"$SHELL\" -l".to_string())
        );
        // The whole script is one argv entry, last, after the destination, and
        // `-t` (before `--`) gives it a tty.
        let cmd = terminal_command(&profile(), Some(&s), askpass(), None, None);
        assert_eq!(
            cmd.args.last().unwrap(),
            "sudo apt update\nuptime\nexec \"$SHELL\" -l"
        );
        assert_eq!(
            cmd.args[cmd.args.len() - 2],
            "alice@example.com",
            "{:?}",
            cmd.args
        );
        assert!(position(&cmd.args, "-t") < position(&cmd.args, "--"));
    }

    #[test]
    fn blank_commands_are_skipped() {
        let s = seq(&["", "uptime", "   ", "\t"]);
        assert_eq!(
            remote_command(Some(&s)),
            Some("uptime\nexec \"$SHELL\" -l".to_string())
        );
        // Nothing but blanks (or nothing at all) means no remote command.
        for commands in [&["", "  ", "\t"][..], &[][..]] {
            let s = seq(commands);
            assert_eq!(remote_command(Some(&s)), None, "{commands:?}");
            let cmd = terminal_command(&profile(), Some(&s), askpass(), None, None);
            assert!(!cmd.args.contains(&"-t".to_string()), "{:?}", cmd.args);
            assert_eq!(cmd.args.last().unwrap(), "alice@example.com");
        }
    }

    #[test]
    fn local_command_runs_the_commands_then_the_same_shell() {
        let seq = StartupSequence {
            name: "u".into(),
            commands: vec![
                "sudo apt update".into(),
                "  ".into(),
                "uptime # check".into(),
                "sleep 1 &".into(),
            ],
        };
        assert_eq!(
            local_command(Some(&seq)).as_deref(),
            Some("sudo apt update\nuptime # check\nsleep 1 &\n\nexec \"$SHELL\"")
        );
        let blank = StartupSequence {
            name: "b".into(),
            commands: vec![" ".into()],
        };
        assert_eq!(local_command(Some(&blank)), None);
        assert_eq!(local_command(None), None);
    }

    #[test]
    fn a_trailing_backslash_cannot_swallow_the_local_exec() {
        // A backslash at the end of a line joins the next line to it: with the
        // exec on the very next line, `printf 'A%s' \` would print "Aexec…" and
        // no shell would be started. The empty line ends the command first.
        let s = seq(&["printf 'A%s' \\"]);
        let script = local_command(Some(&s)).unwrap();
        assert_eq!(script, "printf 'A%s' \\\n\nexec \"$SHELL\"");
        assert!(script.ends_with("\n\nexec \"$SHELL\""), "{script:?}");

        // And so a real shell runs it: the printf, then the exec of `$SHELL`
        // (here `echo`, which prints an empty line).
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&script)
            .env("SHELL", "/bin/echo")
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "A\n");
    }

    #[test]
    fn comment_and_background_commands_cannot_swallow_exec() {
        let s = seq(&["uptime # check", "sleep 1 &"]);
        // Each command sits on its own line, so the trailing comment ends at
        // the newline and the `&` is a complete command, not a syntax error.
        assert_eq!(
            remote_command(Some(&s)),
            Some("uptime # check\nsleep 1 &\nexec \"$SHELL\" -l".to_string())
        );
    }

    #[test]
    fn destination_follows_double_dash() {
        // Deliberately unvalidated: `Profile::new` does not validate, and this
        // proves `--` is a second line of defense behind `Profile::validate`.
        let mut bad_host = profile();
        bad_host.host = "-oProxyCommand=x".to_string();
        let mut bad_user = profile();
        bad_user.username = "-oProxyCommand=x".to_string();

        for p in [&bad_host, &bad_user] {
            let s = seq(&["uptime"]);
            let sync = sync_command(p, askpass(), false, "moniker-server", None);
            let term = terminal_command(p, Some(&s), askpass(), None, None);
            let resolve = resolve_command(p);
            // A jump host's commands, the profile being the jump host.
            let master = jump_master_command(p, socket(), askpass(), None);
            let sync_master = jump_sync_master_command(p, socket(), askpass(), false);
            let check = jump_control_command(p, socket(), JumpControl::Check);
            let exit = jump_control_command(p, socket(), JumpControl::Exit);
            for cmd in [&sync, &term, &resolve, &master, &sync_master, &check, &exit] {
                let dd = position(&cmd.args, "--");
                assert!(
                    cmd.args[..dd].iter().all(|a| !a.contains("oProxyCommand")),
                    "option-like input before `--`: {:?}",
                    cmd.args
                );
                let hits: Vec<usize> = cmd
                    .args
                    .iter()
                    .enumerate()
                    .filter(|(_, a)| a.contains("-oProxyCommand=x"))
                    .map(|(i, _)| i)
                    .collect();
                assert_eq!(hits, vec![dd + 1], "{:?}", cmd.args);
            }
        }
        // The username case is the dangerous one: the whole destination
        // argument starts with `-oProxy...`, so only `--` protects it.
        let cmd = terminal_command(&bad_user, None, askpass(), None, None);
        assert!(cmd.args.last().unwrap().starts_with("-oProxy"));
    }

    #[test]
    fn port_and_key_file_are_separate_argv_entries() {
        let mut p = profile();
        p.port = 2222;
        p.key_file = Some("/home/alice/.ssh/id ed25519".into());
        let cmd = terminal_command(&p, None, askpass(), None, None);
        assert_eq!(
            cmd.args,
            strs(&[
                "-p",
                "2222",
                "-i",
                "/home/alice/.ssh/id ed25519",
                "--",
                "alice@example.com",
            ])
        );
        let s = seq(&["uptime"]);
        let cmd = terminal_command(&p, Some(&s), askpass(), None, None);
        assert_eq!(
            cmd.args,
            strs(&[
                "-p",
                "2222",
                "-i",
                "/home/alice/.ssh/id ed25519",
                "-t",
                "--",
                "alice@example.com",
                "uptime\nexec \"$SHELL\" -l",
            ])
        );
        assert_eq!(cmd.program, "ssh");
    }

    #[test]
    fn resolve_command_asks_ssh_for_the_effective_configuration() {
        let mut p = profile();
        p.port = 2222;
        p.key_file = Some("/k/id".into());
        let cmd = resolve_command(&p);
        assert_eq!(cmd.program, "ssh");
        assert_eq!(
            cmd.args,
            strs(&["-G", "-p", "2222", "-i", "/k/id", "--", "alice@example.com"])
        );
        assert!(cmd.env.is_empty(), "{:?}", cmd.env);
    }

    #[test]
    fn default_port_without_key_file() {
        let cmd = terminal_command(&profile(), None, askpass(), None, None);
        assert_eq!(&cmd.args[..2], &strs(&["-p", "22"])[..]);
        assert!(!cmd.args.contains(&"-i".to_string()));
    }

    fn token() -> AskpassToken {
        AskpassToken::new()
    }

    #[test]
    fn terminal_command_has_no_password_prompt_limit() {
        // A wrong stored password is tried once (the askpass helper's one-time
        // marker sees to that) and then ssh asks in the terminal as usual.
        let s = seq(&["uptime"]);
        let t = token();
        for cmd in [
            terminal_command(&profile(), None, askpass(), None, None),
            terminal_command(&profile(), Some(&s), askpass(), Some(&t), None),
        ] {
            assert!(
                cmd.args
                    .iter()
                    .all(|a| !a.contains("NumberOfPasswordPrompts")),
                "{:?}",
                cmd.args
            );
            assert!(!cmd.args.contains(&"-o".to_string()), "{:?}", cmd.args);
        }
    }

    #[test]
    fn terminal_env_carries_the_token() {
        let p = profile();
        let s = seq(&["uptime"]);
        let t = token();
        let expected: Vec<(String, String)> = vec![
            (
                "SSH_ASKPASS".to_string(),
                "/usr/lib/moniker/moniker-askpass".to_string(),
            ),
            ("SSH_ASKPASS_REQUIRE".to_string(), "force".to_string()),
            ("MONIKER_PROFILE_ID".to_string(), p.id.clone()),
            ("MONIKER_PID".to_string(), std::process::id().to_string()),
            ("MONIKER_ASKPASS_TOKEN".to_string(), t.as_str().to_string()),
        ];
        assert_eq!(
            terminal_command(&p, Some(&s), askpass(), Some(&t), None).env,
            expected
        );
        assert_eq!(
            terminal_command(&p, None, askpass(), Some(&t), None).env,
            expected
        );
        assert_eq!(expected[3].0, PID_ENV);
        assert_eq!(expected[4].0, TOKEN_ENV);
    }

    #[test]
    fn no_token_no_env() {
        let p = profile();
        let s = seq(&["uptime"]);
        assert!(
            terminal_command(&p, None, askpass(), None, None)
                .env
                .is_empty()
        );
        assert!(
            terminal_command(&p, Some(&s), askpass(), None, None)
                .env
                .is_empty()
        );
        assert!(
            sync_command(&p, askpass(), false, "moniker-server", None)
                .env
                .is_empty()
        );
    }

    #[test]
    fn sync_command_still_allows_one_prompt_and_no_token() {
        // Sync has no terminal to fall back to: one prompt, then it fails.
        let p = profile();
        let cmd = sync_command(&p, askpass(), true, "moniker-server", None);
        let limit = position(&cmd.args, "NumberOfPasswordPrompts=1");
        assert_eq!(cmd.args[limit - 1], "-o");
        assert!(limit < position(&cmd.args, "--"));
        assert_eq!(
            cmd.env,
            vec![
                (
                    "SSH_ASKPASS".to_string(),
                    "/usr/lib/moniker/moniker-askpass".to_string(),
                ),
                ("SSH_ASKPASS_REQUIRE".to_string(), "force".to_string()),
                ("MONIKER_PROFILE_ID".to_string(), p.id.clone()),
                ("MONIKER_PID".to_string(), std::process::id().to_string()),
            ]
        );
        assert!(cmd.env.iter().all(|(k, _)| k != TOKEN_ENV));
    }

    #[test]
    fn sync_command_is_non_interactive() {
        let cmd = sync_command(&profile(), askpass(), false, "moniker-server", None);
        assert_eq!(cmd.program, "ssh");
        assert!(cmd.args.contains(&"-T".to_string()), "{:?}", cmd.args);
        assert!(
            cmd.args.contains(&"StrictHostKeyChecking=yes".to_string()),
            "{:?}",
            cmd.args
        );
        assert!(cmd.args.ends_with(&strs(&["moniker-server", "sync-serve"])));
    }

    #[test]
    fn sync_command_has_no_tty_and_strict_host_keys() {
        let mut p = profile();
        p.port = 2200;
        p.key_file = Some("/k/id".into());
        let cmd = sync_command(&p, askpass(), false, "moniker-server", None);
        assert_eq!(
            cmd.args,
            strs(&[
                "-p",
                "2200",
                "-i",
                "/k/id",
                "-T",
                "-o",
                "StrictHostKeyChecking=yes",
                "-o",
                "NumberOfPasswordPrompts=1",
                "--",
                "alice@example.com",
                "moniker-server",
                "sync-serve",
            ])
        );
        assert!(!cmd.args.contains(&"-t".to_string()));
        // The remote command name is two argv entries after the destination.
        let cmd = sync_command(
            &profile(),
            askpass(),
            false,
            "/opt/moniker/bin/moniker-server",
            None,
        );
        assert!(
            cmd.args
                .ends_with(&strs(&["/opt/moniker/bin/moniker-server", "sync-serve"]))
        );
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "tombstone")]
    fn terminal_builder_rejects_tombstones_in_debug_builds() {
        let t = profile().tombstone(1);
        let _ = terminal_command(&t, None, askpass(), None, None);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "tombstone")]
    fn sync_builder_rejects_tombstones_in_debug_builds() {
        let t = profile().tombstone(1);
        let _ = sync_command(&t, askpass(), false, "moniker-server", None);
    }

    // ----- jump hosts -----

    /// A socket as `crate::jump::JumpSocket` makes one.
    const SOCKET: &str = "/run/user/1000/moniker/jump/0123456789abcdef0123456789abcdef";

    fn socket() -> &'static Path {
        Path::new(SOCKET)
    }

    fn bastion() -> Profile {
        let mut p = Profile::new("Bastion", "bastion.example.com", "bob");
        p.port = 2201;
        p
    }

    /// `terminal_command`'s environment for `p` with token `t`.
    fn terminal_env(p: &Profile, t: &AskpassToken) -> Vec<(String, String)> {
        vec![
            (
                "SSH_ASKPASS".to_string(),
                "/usr/lib/moniker/moniker-askpass".to_string(),
            ),
            ("SSH_ASKPASS_REQUIRE".to_string(), "force".to_string()),
            ("MONIKER_PROFILE_ID".to_string(), p.id.clone()),
            ("MONIKER_PID".to_string(), std::process::id().to_string()),
            ("MONIKER_ASKPASS_TOKEN".to_string(), t.as_str().to_string()),
        ]
    }

    /// No option of `cmd` sends the connection through another host: a
    /// `ProxyCommand`, a `ProxyJump` or `-J`.
    fn sets_no_route(cmd: &SshCommand) -> bool {
        cmd.args.iter().all(|a| {
            let a = a.to_lowercase();
            !a.contains("proxycommand") && !a.contains("proxyjump") && a != "-j"
        })
    }

    #[test]
    fn the_master_signs_in_then_goes_to_the_background() {
        assert_eq!(crate::jump::PERSIST_SECS, 10);
        let jump = bastion();
        let cmd = jump_master_command(&jump, socket(), askpass(), None);
        assert_eq!(cmd.program, "ssh");
        assert_eq!(
            cmd.args,
            strs(&[
                "-p",
                "2201",
                "-f",
                "-N",
                "-M",
                "-S",
                SOCKET,
                "-o",
                "ControlPersist=10",
                "--",
                "bob@bastion.example.com",
            ])
        );
        assert!(cmd.env.is_empty(), "{:?}", cmd.env);
        // The jump host's own route in ~/.ssh/config stays: Moniker sets none.
        assert!(sets_no_route(&cmd), "{:?}", cmd.args);

        // With a key file and a stored password: the jump host's own id and
        // the connection's token, as for a terminal tab.
        let mut keyed = jump.clone();
        keyed.key_file = Some("/home/bob/.ssh/id ed25519".into());
        let t = token();
        let cmd = jump_master_command(&keyed, socket(), askpass(), Some(&t));
        assert_eq!(
            cmd.args,
            strs(&[
                "-p",
                "2201",
                "-i",
                "/home/bob/.ssh/id ed25519",
                "-f",
                "-N",
                "-M",
                "-S",
                SOCKET,
                "-o",
                "ControlPersist=10",
                "--",
                "bob@bastion.example.com",
            ])
        );
        assert_eq!(cmd.env, terminal_env(&keyed, &t));
        assert!(sets_no_route(&cmd), "{:?}", cmd.args);
    }

    #[test]
    fn the_sync_master_never_asks_twice() {
        let mut jump = bastion();
        jump.key_file = Some("/k/id".into());
        let cmd = jump_sync_master_command(&jump, socket(), askpass(), true);
        assert_eq!(cmd.program, "ssh");
        assert_eq!(
            cmd.args,
            strs(&[
                "-p",
                "2201",
                "-i",
                "/k/id",
                "-f",
                "-N",
                "-M",
                "-S",
                SOCKET,
                "-o",
                "ControlPersist=10",
                "-o",
                "StrictHostKeyChecking=yes",
                "-o",
                "NumberOfPasswordPrompts=1",
                "--",
                "bob@bastion.example.com",
            ])
        );
        // As `sync_command`'s: the jump host's id and no one-time token.
        assert_eq!(
            cmd.env,
            sync_command(&jump, askpass(), true, "moniker-server", None).env
        );
        assert!(
            cmd.env
                .iter()
                .any(|(k, v)| k == "MONIKER_PROFILE_ID" && *v == jump.id)
        );
        assert!(cmd.env.iter().all(|(k, _)| k != TOKEN_ENV));
        assert!(sets_no_route(&cmd), "{:?}", cmd.args);
        assert!(
            jump_sync_master_command(&jump, socket(), askpass(), false)
                .env
                .is_empty()
        );
    }

    #[test]
    fn the_hop_reaches_the_target_through_the_socket() {
        let expected = format!(
            "ssh -S {SOCKET} -o ControlMaster=no -o BatchMode=yes -W %h:%p -p 2201 \
             -- bob@bastion.example.com"
        );
        assert_eq!(hop_command(&bastion(), socket()), expected);
        // A key file never enters the shell string, and the hop needs none: it
        // goes through the shared connection (or fails, never asking).
        let mut keyed = bastion();
        keyed.key_file = Some("/home/bob/.ssh/id_ed25519".into());
        assert_eq!(hop_command(&keyed, socket()), expected);
    }

    #[test]
    fn a_host_with_a_zone_has_its_percent_doubled() {
        // ssh expands `%` in a ProxyCommand: `%e` would be an unknown token.
        let mut jump = bastion();
        jump.host = "fe80::1%eth0".to_string();
        assert_eq!(
            hop_command(&jump, socket()),
            format!(
                "ssh -S {SOCKET} -o ControlMaster=no -o BatchMode=yes -W %h:%p -p 2201 \
                 -- bob@fe80::1%%eth0"
            )
        );
        // Every `%` of the host, and only those.
        jump.host = "a%b%%c".to_string();
        assert!(hop_command(&jump, socket()).ends_with(" -W %h:%p -p 2201 -- bob@a%%b%%%%c"));
    }

    #[test]
    fn the_target_goes_through_the_proxy_before_its_tty_and_sequence() {
        let hop = hop_command(&bastion(), socket());
        let proxy = format!("ProxyCommand={hop}");
        let p = profile();
        let s = seq(&["uptime"]);
        let t = token();
        let cmd = terminal_command(&p, Some(&s), askpass(), Some(&t), Some(&hop));
        assert_eq!(
            cmd.args,
            strs(&[
                "-p",
                "22",
                "-o",
                &proxy,
                "-t",
                "--",
                "alice@example.com",
                "uptime\nexec \"$SHELL\" -l",
            ])
        );
        // The target's own id and token: the jump host's are the master's.
        assert_eq!(cmd.env, terminal_env(&p, &t));

        // With a key file and no sequence: after `-i`, and no `-t`.
        let mut keyed = profile();
        keyed.key_file = Some("/k/id".into());
        let cmd = terminal_command(&keyed, None, askpass(), None, Some(&hop));
        assert_eq!(
            cmd.args,
            strs(&[
                "-p",
                "22",
                "-i",
                "/k/id",
                "-o",
                &proxy,
                "--",
                "alice@example.com"
            ])
        );
        assert!(cmd.env.is_empty());

        // The ProxyCommand is one argv entry, never split.
        assert_eq!(
            cmd.args
                .iter()
                .filter(|a| a.contains("ProxyCommand"))
                .count(),
            1
        );
    }

    #[test]
    fn a_sync_through_a_jump_host() {
        let hop = hop_command(&bastion(), socket());
        let p = profile();
        let cmd = sync_command(&p, askpass(), true, "moniker-server", Some(&hop));
        assert_eq!(
            cmd.args,
            strs(&[
                "-p",
                "22",
                "-o",
                &format!("ProxyCommand={hop}"),
                "-T",
                "-o",
                "StrictHostKeyChecking=yes",
                "-o",
                "NumberOfPasswordPrompts=1",
                "--",
                "alice@example.com",
                "moniker-server",
                "sync-serve",
            ])
        );
        assert_eq!(
            cmd.env,
            sync_command(&p, askpass(), true, "moniker-server", None).env
        );
    }

    #[test]
    fn control_commands_carry_no_environment() {
        let mut jump = bastion();
        jump.key_file = Some("/k/id".into());
        for (op, word) in [(JumpControl::Check, "check"), (JumpControl::Exit, "exit")] {
            let cmd = jump_control_command(&jump, socket(), op);
            assert_eq!(cmd.program, "ssh");
            assert_eq!(
                cmd.args,
                strs(&[
                    "-S",
                    SOCKET,
                    "-O",
                    word,
                    "-p",
                    "2201",
                    "--",
                    "bob@bastion.example.com",
                ])
            );
            assert!(cmd.env.is_empty(), "{:?}", cmd.env);
        }
    }

    #[test]
    fn no_builder_carries_a_password() {
        // A builder takes no password, so none can be in argv or the
        // environment. What a profile carries besides its connection (here its
        // name) goes nowhere either, and the environment holds only the
        // helper's non-secret variables.
        const SECRET: &str = "correct horse battery staple";
        let named = |mut p: Profile| {
            p.name = SECRET.to_string();
            p
        };
        let (p, jump) = (named(profile()), named(bastion()));
        let s = seq(&["uptime"]);
        let t = token();
        let hop = hop_command(&jump, socket());
        let commands = [
            terminal_command(&p, Some(&s), askpass(), Some(&t), Some(&hop)),
            sync_command(&p, askpass(), true, "moniker-server", Some(&hop)),
            resolve_command(&p),
            jump_master_command(&jump, socket(), askpass(), Some(&t)),
            jump_sync_master_command(&jump, socket(), askpass(), true),
            jump_control_command(&jump, socket(), JumpControl::Check),
            jump_control_command(&jump, socket(), JumpControl::Exit),
        ];
        let allowed = [
            "SSH_ASKPASS",
            "SSH_ASKPASS_REQUIRE",
            "MONIKER_PROFILE_ID",
            PID_ENV,
            TOKEN_ENV,
        ];
        assert!(!hop.contains(SECRET), "{hop}");
        for cmd in &commands {
            assert!(
                cmd.args.iter().all(|a| !a.contains(SECRET)),
                "{:?}",
                cmd.args
            );
            for (name, value) in &cmd.env {
                assert!(allowed.contains(&name.as_str()), "{name}");
                assert!(!value.contains(SECRET), "{name}");
            }
        }
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "tombstone")]
    fn jump_builders_reject_tombstones_in_debug_builds() {
        let t = bastion().tombstone(1);
        let _ = jump_master_command(&t, socket(), askpass(), None);
    }
}
