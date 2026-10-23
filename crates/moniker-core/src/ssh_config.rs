//! What `~/.ssh/config` makes of a profile: `ssh -G`.
//!
//! `ssh -G` prints the configuration `ssh` would use for a connection, after
//! `~/.ssh/config` and `/etc/ssh/ssh_config` are applied, and exits without
//! connecting. Moniker reads two things from it: the user and host a password
//! prompt will name (the askpass helper, `crate::askpass::effective_identity`)
//! and the host a connection really goes to, to ping it ([`ping_target`]).
//!
//! [`effective_config`] runs it like every `ssh` Moniker starts: the system
//! `ssh` from `PATH`, from argv (`crate::ssh::resolve_command`, with `--`
//! before the destination), never through a shell. It gets no input, its
//! errors are discarded, it has [`CONFIG_LIMIT`] to answer, and it runs
//! without the variables that lead `ssh` to the askpass helper: it never
//! prompts, and if it ever did, that must not come back to the helper.
//! Nothing it prints is logged.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::askpass::{PID_ENV, TOKEN_ENV};
use crate::profile::Profile;
use crate::ssh::resolve_command;

/// How long `ssh -G` may take. It normally answers in milliseconds; only a
/// `Match exec` or host name canonicalization in `~/.ssh/config` can make it
/// slower.
pub const CONFIG_LIMIT: Duration = Duration::from_secs(5);

/// The most of `ssh -G`'s output that is read: it prints a few kilobytes.
const CONFIG_MAX_BYTES: u64 = 256 * 1024;

/// The variables that lead `ssh` to the askpass helper, kept from `ssh -G`.
const HELPER_ENV: [&str; 5] = [
    "SSH_ASKPASS",
    "SSH_ASKPASS_REQUIRE",
    "MONIKER_PROFILE_ID",
    PID_ENV,
    TOKEN_ENV,
];

/// What `ssh -G` prints for the connection to `profile`, if it answers within
/// [`CONFIG_LIMIT`] (module docs).
pub fn effective_config(profile: &Profile) -> Option<String> {
    output_within(config_process(profile), CONFIG_LIMIT)
}

/// `ssh -G` for the connection to `p`, without the askpass helper's variables.
fn config_process(p: &Profile) -> Command {
    let ssh = resolve_command(p);
    let mut command = Command::new(&ssh.program);
    command.args(&ssh.args);
    for name in HELPER_ENV {
        command.env_remove(name);
    }
    command
}

/// What `command` prints, if it exits successfully within `limit` and prints
/// valid UTF-8, at most [`CONFIG_MAX_BYTES`] of it (more is not trusted: a dump
/// cut short could hide a repeated key). It gets no input and its errors are
/// discarded. A command still running at `limit` is killed; one that exits but
/// leaves a process holding its output open is given up on at `limit` too.
fn output_within(mut command: Command, limit: Duration) -> Option<String> {
    let deadline = Instant::now() + limit;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let (sender, received) = mpsc::channel();
    thread::spawn(move || {
        let mut out = Vec::new();
        // One byte more than the limit, to tell a full dump from a long one.
        let read = stdout.take(CONFIG_MAX_BYTES + 1).read_to_end(&mut out);
        let _ = sender.send(read.map(|_| out));
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
    let out = received
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok()?
        .ok()?;
    if out.len() as u64 > CONFIG_MAX_BYTES {
        return None;
    }
    String::from_utf8(out).ok()
}

/// Where a ping for a profile goes, as `ssh -G` says ([`ping_target`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PingTarget {
    /// This host, which the connection goes to directly.
    Host(String),
    /// The connection goes through another host (`ProxyJump` or
    /// `ProxyCommand`): this machine may not reach the host itself, so a ping
    /// from here says nothing about it.
    ThroughAnotherHost,
}

/// Where a ping for a connection goes, read from what `ssh -G` printed for it
/// ([`effective_config`]): the `hostname` (after `HostName` and aliases; a
/// `HostKeyAlias` changes nothing), unless a `proxyjump` or `proxycommand`
/// other than `none` sends the connection through another host. `None` if the
/// dump does not say: no `hostname`, more than one, or one that is not a single
/// word without control characters.
pub fn ping_target(dump: &str) -> Option<PingTarget> {
    let mut hostname = None;
    let mut through = false;
    for line in dump.lines() {
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        match key {
            "hostname" => {
                if hostname.replace(value).is_some() {
                    return None;
                }
            }
            "proxyjump" | "proxycommand" => through |= value != "none",
            _ => {}
        }
    }
    let hostname = hostname.filter(|host| {
        !host.is_empty() && !host.chars().any(|c| c.is_whitespace() || c.is_control())
    })?;
    Some(if through {
        PingTarget::ThroughAnotherHost
    } else {
        PingTarget::Host(hostname.to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(script);
        command
    }

    // ----- running ssh -G -----

    #[test]
    fn a_quick_commands_output_is_read() {
        assert_eq!(
            output_within(
                sh("printf 'user a\\nhostname b\\n'"),
                Duration::from_secs(5)
            )
            .as_deref(),
            Some("user a\nhostname b\n")
        );
    }

    #[test]
    fn a_failed_command_gives_nothing() {
        assert_eq!(
            output_within(sh("printf 'user a\\n'; exit 3"), Duration::from_secs(5)),
            None
        );
        assert_eq!(
            output_within(Command::new("/nonexistent/ssh"), Duration::from_secs(5)),
            None
        );
    }

    #[test]
    fn a_slow_command_is_given_up_on() {
        let start = Instant::now();
        assert_eq!(
            output_within(sh("sleep 5"), Duration::from_millis(200)),
            None
        );
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn output_held_open_by_a_leftover_process_is_given_up_on() {
        // The command exits, but a process it left behind keeps its output open.
        let start = Instant::now();
        assert_eq!(
            output_within(
                sh("sleep 5 & printf 'user a\\n'"),
                Duration::from_millis(300)
            ),
            None
        );
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn output_beyond_the_limit_is_not_trusted() {
        // A dump cut short could hide a repeated key.
        let print = |n: u64| sh(&format!("head -c {n} /dev/zero | tr '\\0' x"));
        assert_eq!(
            output_within(print(CONFIG_MAX_BYTES + 1), Duration::from_secs(5)),
            None
        );
        assert_eq!(
            output_within(print(CONFIG_MAX_BYTES), Duration::from_secs(5)).map(|out| out.len()),
            Some(CONFIG_MAX_BYTES as usize)
        );
    }

    #[test]
    fn ssh_g_runs_from_argv_without_the_helpers_variables() {
        let mut p = Profile::new("web1", "web", "admin");
        p.port = 2222;
        let command = config_process(&p);
        assert_eq!(command.get_program(), "ssh");
        let args: Vec<&std::ffi::OsStr> = command.get_args().collect();
        assert_eq!(args, ["-G", "-p", "2222", "--", "admin@web"]);
        let removed: Vec<&std::ffi::OsStr> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| name)
            .collect();
        for name in [
            "SSH_ASKPASS",
            "SSH_ASKPASS_REQUIRE",
            "MONIKER_PROFILE_ID",
            "MONIKER_PID",
            "MONIKER_ASKPASS_TOKEN",
        ] {
            assert!(removed.contains(&std::ffi::OsStr::new(name)), "{name}");
        }
        assert!(command.get_envs().all(|(_, value)| value.is_none()));
    }

    // ----- where a ping goes -----

    /// Part of what `ssh -G` prints, as OpenSSH 10.2 does.
    const DUMP: &str = "user alice\nhostname web1.example.com\nport 22\n\
        identityfile ~/.ssh/id_ed25519\nsendenv LANG\n";

    #[test]
    fn a_ping_goes_to_the_host_ssh_connects_to() {
        assert_eq!(
            ping_target(DUMP),
            Some(PingTarget::Host("web1.example.com".to_owned()))
        );
        // A HostKeyAlias names the host key, not where ssh connects.
        let dump = format!("{DUMP}hostkeyalias myalias\n");
        assert_eq!(
            ping_target(&dump),
            Some(PingTarget::Host("web1.example.com".to_owned()))
        );
        // `none` is no proxy.
        let dump = format!("{DUMP}proxyjump none\nproxycommand none\n");
        assert_eq!(
            ping_target(&dump),
            Some(PingTarget::Host("web1.example.com".to_owned()))
        );
    }

    #[test]
    fn a_host_reached_through_another_is_not_pinged() {
        for proxy in [
            "proxyjump bastion.example.com",
            "proxyjump alice@bastion:2222,other",
            "proxycommand ssh -W %h:%p bastion",
        ] {
            let dump = format!("{DUMP}{proxy}\n");
            assert_eq!(
                ping_target(&dump),
                Some(PingTarget::ThroughAnotherHost),
                "{proxy}"
            );
        }
    }

    #[test]
    fn an_unclear_dump_gives_no_target() {
        for dump in [
            "",
            "user alice\nport 22\n",
            "hostname a.example.com\nhostname b.example.com\n",
            "hostname web1 example.com\n",
            "hostname\n",
            "hostname web1\u{7}\n",
        ] {
            assert_eq!(ping_target(dump), None, "{dump:?}");
        }
    }
}
