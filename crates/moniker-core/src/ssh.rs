//! Building the `ssh` command line for a [`Profile`].
//!
//! Pure functions: nothing here spawns a process or touches the keyring. The
//! GUI spawns [`terminal_command`] inside a VTE tab and the sync code spawns
//! [`sync_command`] headlessly.
//!
//! The builders take no password, so none can reach argv or the environment
//! (both are visible through `/proc`). When a password is stored, `ssh` is
//! pointed at the `moniker-askpass` helper through `SSH_ASKPASS`; only the
//! non-secret profile id (and, for a terminal tab, the non-secret one-time
//! token, see `crate::askpass`) travels in the environment, and the helper looks
//! the password up in the keyring itself.
//!
//! The builders assume a validated live profile (see `Profile::validate`).
//! The `--` separator before the destination is a second line of defence in
//! case a bad host or username ever slips through: after `--`, `ssh` cannot
//! parse the destination as an option.
//!
//! Paths (`key_file`, the askpass helper) are converted with
//! [`Path::to_string_lossy`]. A path that is not valid UTF-8 is unsupported:
//! the replacement characters would name a different file.

use std::path::Path;

use crate::askpass::{AskpassToken, TOKEN_ENV};
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
    let commands: Vec<&str> = seq?
        .commands
        .iter()
        .map(String::as_str)
        .filter(|c| !c.trim().is_empty())
        .collect();
    if commands.is_empty() {
        return None;
    }
    Some(format!("{}\nexec \"$SHELL\" -l", commands.join("\n")))
}

/// `ssh` invocation for an interactive terminal tab.
///
/// Arguments, in order: `-p <port>`, `-i <key>` if the profile has a key file,
/// `-t` only if there is a remote command (so the startup sequence gets a tty),
/// `--`, `<username>@<host>`, then the remote command as a single argument if
/// any.
///
/// There is no limit on password prompts: `ssh` allows its usual three. The
/// askpass helper answers at most one of them with the stored password (the
/// one-time marker named by `token`, see `crate::askpass`); after a wrong one
/// the user types the password in the terminal, as with plain `ssh`.
///
/// With a `token` (a password is stored) the environment points `ssh` at the
/// askpass helper: exactly `SSH_ASKPASS=<askpass>`, `SSH_ASKPASS_REQUIRE=force`,
/// `MONIKER_PROFILE_ID=<profile id>` and `MONIKER_ASKPASS_TOKEN=<token>`, in
/// that order. Without one it is empty. Neither the profile id nor the token is
/// secret.
pub fn terminal_command(
    p: &Profile,
    seq: Option<&StartupSequence>,
    askpass: &Path,
    token: Option<&AskpassToken>,
) -> SshCommand {
    debug_assert!(!p.deleted, "ssh command requested for a tombstone");
    let remote = remote_command(seq);
    let mut args = connection_args(p);
    if remote.is_some() {
        args.push("-t".to_string());
    }
    push_destination(&mut args, p);
    args.extend(remote);
    let env = match token {
        Some(token) => {
            let mut env = askpass_env(p, askpass, true);
            env.push((TOKEN_ENV.to_string(), token.as_str().to_string()));
            env
        }
        None => Vec::new(),
    };
    SshCommand {
        program: SSH_PROGRAM.to_string(),
        args,
        env,
    }
}

/// `ssh` invocation that runs `<remote_server> sync-serve` on the profile's
/// host with no tty.
///
/// Arguments, in order: `-p <port>`, `-i <key>` if the profile has a key file,
/// `-T`, `-o StrictHostKeyChecking=yes`, `-o NumberOfPasswordPrompts=1`, `--`,
/// `<username>@<host>`, `<remote_server>`, `sync-serve`. With no tty, an
/// unknown host key fails instead of prompting.
///
/// `remote_server` is the remote command name or path, normally
/// [`SERVER_PROGRAM`]. It is trusted, code-supplied input: it is not quoted and
/// the remote shell splits it, so it must never come from a profile or the
/// network.
///
/// With `has_password` the environment points `ssh` at the askpass helper:
/// exactly `SSH_ASKPASS=<askpass>`, `SSH_ASKPASS_REQUIRE=force` and
/// `MONIKER_PROFILE_ID=<profile id>`, in that order, and no one-time token: a
/// sync has no terminal to fall back to, so `NumberOfPasswordPrompts=1` is what
/// keeps a wrong stored password from being sent twice. Otherwise it is empty.
pub fn sync_command(
    p: &Profile,
    askpass: &Path,
    has_password: bool,
    remote_server: &str,
) -> SshCommand {
    debug_assert!(!p.deleted, "ssh command requested for a tombstone");
    let mut args = connection_args(p);
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

/// The `--` separator and then `<username>@<host>`, so that neither can be
/// parsed as an `ssh` option.
fn push_destination(args: &mut Vec<String>, p: &Profile) {
    args.push("--".to_string());
    args.push(format!("{}@{}", p.username, p.host));
}

/// The environment that makes `ssh` ask `moniker-askpass` for the password:
/// empty when no password is stored. Carries only the non-secret profile id;
/// [`terminal_command`] adds its token.
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
    ]
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
        let cmd = terminal_command(&profile(), None, askpass(), None);
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
        let cmd = terminal_command(&profile(), Some(&s), askpass(), None);
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
            let cmd = terminal_command(&profile(), Some(&s), askpass(), None);
            assert!(!cmd.args.contains(&"-t".to_string()), "{:?}", cmd.args);
            assert_eq!(cmd.args.last().unwrap(), "alice@example.com");
        }
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
        // proves `--` is a second line of defence behind `Profile::validate`.
        let mut bad_host = profile();
        bad_host.host = "-oProxyCommand=x".to_string();
        let mut bad_user = profile();
        bad_user.username = "-oProxyCommand=x".to_string();

        for p in [&bad_host, &bad_user] {
            let s = seq(&["uptime"]);
            let sync = sync_command(p, askpass(), false, "moniker-server");
            let term = terminal_command(p, Some(&s), askpass(), None);
            for cmd in [&sync, &term] {
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
        let cmd = terminal_command(&bad_user, None, askpass(), None);
        assert!(cmd.args.last().unwrap().starts_with("-oProxy"));
    }

    #[test]
    fn port_and_key_file_are_separate_argv_entries() {
        let mut p = profile();
        p.port = 2222;
        p.key_file = Some("/home/alice/.ssh/id ed25519".into());
        let cmd = terminal_command(&p, None, askpass(), None);
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
        let cmd = terminal_command(&p, Some(&s), askpass(), None);
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
    fn default_port_without_key_file() {
        let cmd = terminal_command(&profile(), None, askpass(), None);
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
            terminal_command(&profile(), None, askpass(), None),
            terminal_command(&profile(), Some(&s), askpass(), Some(&t)),
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
            ("MONIKER_ASKPASS_TOKEN".to_string(), t.as_str().to_string()),
        ];
        assert_eq!(
            terminal_command(&p, Some(&s), askpass(), Some(&t)).env,
            expected
        );
        assert_eq!(
            terminal_command(&p, None, askpass(), Some(&t)).env,
            expected
        );
        assert_eq!(expected[3].0, TOKEN_ENV);
    }

    #[test]
    fn no_token_no_env() {
        let p = profile();
        let s = seq(&["uptime"]);
        assert!(terminal_command(&p, None, askpass(), None).env.is_empty());
        assert!(
            terminal_command(&p, Some(&s), askpass(), None)
                .env
                .is_empty()
        );
        assert!(
            sync_command(&p, askpass(), false, "moniker-server")
                .env
                .is_empty()
        );
    }

    #[test]
    fn sync_command_still_allows_one_prompt_and_no_token() {
        // Sync has no terminal to fall back to: one prompt, then it fails.
        let p = profile();
        let cmd = sync_command(&p, askpass(), true, "moniker-server");
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
            ]
        );
        assert!(cmd.env.iter().all(|(k, _)| k != TOKEN_ENV));
    }

    #[test]
    fn sync_command_is_non_interactive() {
        let cmd = sync_command(&profile(), askpass(), false, "moniker-server");
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
        let cmd = sync_command(&p, askpass(), false, "moniker-server");
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
        let _ = terminal_command(&t, None, askpass(), None);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "tombstone")]
    fn sync_builder_rejects_tombstones_in_debug_builds() {
        let t = profile().tombstone(1);
        let _ = sync_command(&t, askpass(), false, "moniker-server");
    }
}
