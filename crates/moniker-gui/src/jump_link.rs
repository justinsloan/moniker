//! A pane's shared connection to its jump host (issue #9), and how the pane
//! ends it.
//!
//! A pane whose profile goes through a jump host runs two `ssh`s one after the
//! other on its terminal (`crate::terminal_tab`): first the master, which signs
//! in to the jump host as Moniker's own child and then goes to the background,
//! serving a shared connection at a private socket; then the target's, whose
//! `ProxyCommand` asks that master for a channel to the target
//! (`moniker_core::jump::pane_commands` builds all of them). Once in the
//! background the master is no child of Moniker, so nothing ends it with the
//! pane by itself: the pane keeps a [`JumpLink`] for that, with the master's
//! socket and the `-O check` and `-O exit` commands as they were built when the
//! pane connected. A jump host edited or deleted since changes nothing of how
//! its master is ended.
//!
//! * Once the master has signed in, [`JumpLink::learn_pid`] asks it for its
//!   process id and holds that process by a pidfd
//!   (`moniker_core::jump::learn_master`). The commands are processes to wait
//!   for, so they run on a worker thread, never on the main thread.
//! * [`JumpLink::close`] ends the master when the pane's connection ends (its
//!   `ssh` exits, or the pane, its tab or its window closes): `SIGTERM` through
//!   the pidfd when it is held, a system call that also works as the
//!   application quits, else `ssh -O exit` through the socket, on a worker
//!   thread, with the application held until it is done. An answer of
//!   `learn_pid` that comes back after the link was closed ends the master
//!   then: the master that signed in while its pane was closing is ended too.
//! * Should neither reach it (Moniker was killed), the master ends by itself
//!   `moniker_core::jump::PERSIST_SECS` after its last channel closed.
//!
//! One link per pane (the user's decision): two panes through the same jump
//! host each have a master of their own, and closing one ends only its own.
//! The link lives in the pane's holder (`crate::terminal_tab::PaneHolder::jump`),
//! so it moves with the pane to another tab or window. Every command goes
//! through `crate::terminal_tab::test_hook::command`, so the window tests run
//! them with a fake `ssh`.

use std::cell::{Cell, RefCell};
use std::os::fd::OwnedFd;
#[cfg(test)]
use std::path::Path;
use std::rc::Rc;
use std::thread;

use gtk::prelude::*;
use gtk::{gio, glib};
use gtk4 as gtk;
use moniker_core::jump::{CONTROL_LIMIT, JumpSocket, end_master, learn_master, run_control};
use moniker_core::ssh::SshCommand;

use crate::terminal_tab::test_hook;

/// The shared connection of one pane's connection through its jump host: what
/// ends its master (module docs).
pub(crate) struct JumpLink {
    /// The master's socket. One per link, never reused; its path is in `check`
    /// and `exit`.
    #[cfg_attr(not(test), allow(dead_code))]
    socket: JumpSocket,
    /// `ssh -S <socket> -O check …`, which prints the master's process id.
    check: SshCommand,
    /// `ssh -S <socket> -O exit …`, which tells the master to end.
    exit: SshCommand,
    /// The master, once [`learn_pid`](Self::learn_pid) has learned it, until it
    /// is ended.
    pidfd: RefCell<Option<OwnedFd>>,
    /// The pane's connection has ended: the master is, or is being, ended.
    closed: Cell<bool>,
}

impl JumpLink {
    /// The link of the master at `socket`, with the control commands built for
    /// it (`moniker_core::jump::PaneCommands`).
    pub(crate) fn new(socket: JumpSocket, check: SshCommand, exit: SshCommand) -> JumpLink {
        JumpLink {
            socket,
            check,
            exit,
            pidfd: RefCell::new(None),
            closed: Cell::new(false),
        }
    }

    /// Learn the master's process id, now that it has signed in, on a worker
    /// thread (`moniker_core::jump::learn_master`), and back on the main loop
    /// keep its pidfd for [`close`](Self::close). If the link was closed
    /// meanwhile, end the master then instead: through the pidfd, or with `ssh
    /// -O exit` when it could not be learned (the link may have been closed
    /// before the master's socket existed, while it signed in).
    ///
    /// A master that cannot be learned while the link is open is ended with
    /// `-O exit` when the link closes.
    pub(crate) fn learn_pid(self: &Rc<Self>) {
        let check = test_hook::command(self.check.clone());
        let link = Rc::clone(self);
        on_a_worker(
            move || learn_master(&check),
            move |pidfd| {
                let pidfd = pidfd.flatten();
                if !link.closed.get() {
                    link.pidfd.replace(pidfd);
                    return;
                }
                match pidfd {
                    Some(pidfd) => {
                        // An error: it is gone already.
                        let _ = end_master(&pidfd);
                    }
                    None => link.tell_exit(),
                }
            },
        );
    }

    /// End the master, once: through its pidfd when it is known, else with
    /// `ssh -O exit` on a worker thread (module docs). Called when the pane's
    /// connection ends; the second call and later ones do nothing.
    pub(crate) fn close(&self) {
        if self.closed.replace(true) {
            return;
        }
        match self.pidfd.take() {
            Some(pidfd) => {
                // An error: it is gone already.
                let _ = end_master(&pidfd);
            }
            None => self.tell_exit(),
        }
    }

    /// Run `-O exit` on a worker thread, with the application held until it is
    /// done, so that closing the last window does not quit before the master
    /// was told. It answers in milliseconds, and within
    /// `moniker_core::jump::CONTROL_LIMIT` in any case.
    fn tell_exit(&self) {
        let exit = test_hook::command(self.exit.clone());
        let hold = gio::Application::default().map(|app| app.hold());
        on_a_worker(
            move || {
                let _ = run_control(&exit, CONTROL_LIMIT);
            },
            move |_| drop(hold),
        );
    }

    /// Whether the master's pidfd is held (tests).
    #[cfg(test)]
    pub(crate) fn knows_master(&self) -> bool {
        self.pidfd.borrow().is_some()
    }

    /// Whether [`close`](Self::close) was called (tests).
    #[cfg(test)]
    pub(crate) fn is_closed(&self) -> bool {
        self.closed.get()
    }

    /// The master's socket (tests).
    #[cfg(test)]
    pub(crate) fn socket(&self) -> &Path {
        self.socket.path()
    }
}

impl Drop for JumpLink {
    /// A link nobody holds any more ends its master, should its pane have gone
    /// without ending its connection: never left to the backstop when Moniker
    /// can end it.
    fn drop(&mut self) {
        self.close();
    }
}

/// Run `work` on a thread of its own and `done` on the main loop with what it
/// returned, or with `None` when the thread could not be started or `work`
/// panicked.
fn on_a_worker<R: Send + 'static>(
    work: impl FnOnce() -> R + Send + 'static,
    done: impl FnOnce(Option<R>) + 'static,
) {
    let (sender, received) = async_channel::bounded::<R>(1);
    // A thread that cannot be started drops `work` and its sender here, so the
    // receive below ends at once with nothing.
    let _ = thread::Builder::new()
        .name("moniker-jump".to_owned())
        .spawn(move || {
            let _ = sender.send_blocking(work());
        });
    glib::MainContext::default().spawn_local(async move {
        done(received.recv().await.ok());
    });
}
