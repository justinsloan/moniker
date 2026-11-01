//! Whether a session runs as root (issue #17): what the root indicator decides,
//! apart from showing it, so that it is testable without a display.
//!
//! * **A local pane** is root while the process group in the foreground of its
//!   terminal runs as root, which is Ptyxis's rule ([`foreground_is_root`]). The
//!   test is the group's **effective uid**, read from the `Uid:` line of
//!   `/proc/<pid>/status` ([`effective_uid_is_root`]), not whether the folder
//!   `/proc/<pid>/` belongs to root as Ptyxis tests it: that folder is also
//!   root's for any process that made itself non-dumpable, which some ordinary
//!   programs do for safety (agents, password tools), and they would show red.
//!   `sudo` counts from the start, because it is a setuid program: its
//!   effective uid is 0 even while it asks for the password.
//! * **A profile pane** runs `ssh`, which always runs as the person, so nothing
//!   is read from `/proc` for it: the profile's own user name decides
//!   ([`logs_in_as_root`]), the one thing Moniker knows for certain about the
//!   remote session.
//! * **When to look** is Ptyxis's poll ([`Backoff`]): soon after output, then
//!   less and less often while nothing changes.
//!
//! Nothing here is per frame, and nothing here reads a secret.

use std::os::fd::BorrowedFd;
use std::time::Duration;

use rustix::termios::tcgetpgrp;

/// The tooltip and accessible label of every root mark.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const ROOT_TOOLTIP: &str = "Running as root";

/// The icon of the root mark, before the title in the header bar and in a
/// pane's header, and as the tab's icon.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const ROOT_ICON: &str = "dialog-warning-symbolic";

/// Whether the text of a `/proc/<pid>/status` file says the process runs as
/// root: the second number (the effective uid) of the first `Uid:` line is 0.
///
/// The line has four numbers, the real, effective, saved and file system ids.
/// A setuid program such as `sudo` has a real id of the person and an effective
/// id of 0, and is root; a root process that dropped to a person has the
/// opposite and is not. A file without such a line, or with one that does not
/// parse, is not root.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn effective_uid_is_root(status: &str) -> bool {
    status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|ids| ids.split_whitespace().nth(1))
        .and_then(|effective| effective.parse::<u32>().ok())
        == Some(0)
}

/// Whether process `pid` runs as root, by its `/proc/<pid>/status`. A pid of 0
/// or below (a group outside our pid namespace has none), a process that is
/// gone and a file that cannot be read are not root.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn pid_is_root(pid: i32) -> bool {
    pid > 0
        && std::fs::read_to_string(format!("/proc/{pid}/status"))
            .is_ok_and(|status| effective_uid_is_root(&status))
}

/// Whether the process group in the foreground of the terminal whose pty is
/// `pty` runs as root, by its leader's id (a group's id is its leader's pid).
/// Anything that is no terminal, or has no foreground group, is not root.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn foreground_is_root(pty: BorrowedFd<'_>) -> bool {
    tcgetpgrp(pty).is_ok_and(|group| pid_is_root(group.as_raw_pid()))
}

/// Whether a profile signs in as root: its user name is exactly `root`. An
/// account with uid 0 under another name is not seen.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn logs_in_as_root(user: &str) -> bool {
    user == "root"
}

/// How long to wait before the next check of a local pane: the schedule of
/// Ptyxis's tab monitor (`ptyxis-tab-monitor.c`), kept per pane.
///
/// A check comes [`Backoff::FIRST`] after the terminal's first output. A check
/// that found no change doubles the delay, up to [`Backoff::MAX`]: 500 ms, 1 s,
/// 2 s, 4 s, 8 s, 10 s. A check that found a change sets it back to
/// [`Backoff::FIRST`], and so does output while the delay is longer
/// ([`Backoff::output`]). Enter, Ctrl+C and Ctrl+D pull the next check in to
/// [`Backoff::HURRY`] ([`Backoff::hurry`]).
///
/// It only counts delays; the caller owns the timer.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct Backoff {
    delay: Duration,
}

#[cfg_attr(not(test), allow(dead_code))]
impl Backoff {
    /// The first delay, and the shortest one after a change or output.
    pub(crate) const FIRST: Duration = Duration::from_millis(500);
    /// The longest delay.
    pub(crate) const MAX: Duration = Duration::from_secs(10);
    /// The delay after Enter, Ctrl+C or Ctrl+D.
    pub(crate) const HURRY: Duration = Duration::from_millis(100);

    /// A schedule that waits [`Backoff::FIRST`].
    pub(crate) fn new() -> Backoff {
        Backoff { delay: Self::FIRST }
    }

    /// The current delay.
    pub(crate) fn delay(&self) -> Duration {
        self.delay
    }

    /// A check has run: `changed` says whether it found the root state
    /// different. Returns the delay before the next one.
    pub(crate) fn checked(&mut self, changed: bool) -> Duration {
        self.delay = if changed {
            Self::FIRST
        } else {
            (self.delay * 2).clamp(Self::FIRST, Self::MAX)
        };
        self.delay
    }

    /// The terminal printed something. When the delay is longer than
    /// [`Backoff::FIRST`] it goes back to it and this returns true: the caller
    /// moves its timer. At [`Backoff::FIRST`] (or shorter, after a hurry)
    /// nothing changes and this returns false.
    pub(crate) fn output(&mut self) -> bool {
        if self.delay > Self::FIRST {
            self.delay = Self::FIRST;
            true
        } else {
            false
        }
    }

    /// Enter, Ctrl+C or Ctrl+D was typed: the next check comes in
    /// [`Backoff::HURRY`]. Returns that delay.
    pub(crate) fn hurry(&mut self) -> Duration {
        self.delay = Self::HURRY;
        self.delay
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mark_says_what_the_design_says() {
        assert_eq!(ROOT_TOOLTIP, "Running as root");
        assert_eq!(ROOT_ICON, "dialog-warning-symbolic");
    }

    #[test]
    fn the_effective_uid_decides() {
        assert!(effective_uid_is_root(
            "Name:\tbash\nUid:\t0\t0\t0\t0\nGid:\t0\t0\t0\t0\n"
        ));
        // setuid: real 1000, effective 0 (sudo asking for the password)
        assert!(effective_uid_is_root("Uid:\t1000\t0\t1000\t1000\n"));
        // real 0, effective 1000: not root
        assert!(!effective_uid_is_root("Uid:\t0\t1000\t0\t0\n"));
        assert!(effective_uid_is_root("Uid:   0   0 0 0\n"));
        assert!(!effective_uid_is_root("Name:\tbash\n"));
        assert!(!effective_uid_is_root("Uid:\n"));
        assert!(!effective_uid_is_root("Uid:\tx\ty\n"));
        assert!(!effective_uid_is_root(""));
        // only the first Uid: line counts
        assert!(!effective_uid_is_root(
            "Uid:\t1000\t1000\t1000\t1000\nUid:\t0\t0\t0\t0\n"
        ));
    }

    #[test]
    fn this_process_is_read_as_it_is() {
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        assert_eq!(
            effective_uid_is_root(&status),
            rustix::process::geteuid().is_root()
        );
        assert_eq!(
            pid_is_root(rustix::process::getpid().as_raw_pid()),
            rustix::process::geteuid().is_root()
        );
    }

    #[test]
    fn a_pid_with_no_proc_entry_is_not_root() {
        assert!(!pid_is_root(0));
        assert!(!pid_is_root(-1));
        assert!(!pid_is_root(0x3fff_ffff)); // above Linux's pid_max ceiling
    }

    #[test]
    fn something_that_is_no_terminal_is_not_root() {
        let null = std::fs::File::open("/dev/null").unwrap();
        assert!(!foreground_is_root(std::os::fd::AsFd::as_fd(&null)));
    }

    #[test]
    fn only_the_user_root_logs_in_as_root() {
        assert!(logs_in_as_root("root"));
        for user in ["Root", "ROOT", "rootx", " root", "root ", "", "toor"] {
            assert!(!logs_in_as_root(user), "{user:?}");
        }
    }

    #[test]
    fn the_schedule_is_ptyxis_s() {
        let ms = Duration::from_millis;
        let mut b = Backoff::new();
        assert_eq!(b.delay(), ms(500));
        let grown: Vec<_> = (0..6).map(|_| b.checked(false)).collect();
        assert_eq!(
            grown,
            [
                ms(1000),
                ms(2000),
                ms(4000),
                ms(8000),
                ms(10_000),
                ms(10_000)
            ]
        );
        assert_eq!(b.checked(true), ms(500));
        assert!(!b.output(), "at 500 ms output changes nothing");
        b.checked(false); // 1000
        assert!(b.output());
        assert_eq!(b.delay(), ms(500));
        assert_eq!(b.hurry(), ms(100));
        assert_eq!(
            b.checked(false),
            ms(500),
            "doubling never goes below 500 ms"
        );
        b.hurry();
        assert_eq!(b.checked(true), ms(500));
    }
}
