//! The GUI's one way to the system keyring, and to writing the profile file:
//! a worker thread.
//!
//! A Secret Service call can take long. A locked keyring shows its unlock
//! prompt and waits for the person, and a slow or stuck service answers late.
//! On the main thread that would freeze every window and stop every terminal
//! from redrawing, so every keyring call of the GUI goes through [`read`] or
//! [`change`]: the call runs on one worker thread with the app's secret store,
//! and what it returns comes back to a callback on the main thread, from the
//! GLib main loop. (A sync's worker reads the keyring on its own thread: `crate::sync`.)
//!
//! * **One thread, in order.** The calls run one after another, in the order
//!   they were asked for: a password saved and then removed ends up removed,
//!   and a later call never overtakes an earlier one stuck at an unlock prompt.
//! * **The saving order is kept.** A call that also writes the profile file
//!   writes it on the worker, after the keyring, through a handle of its own on
//!   the file ([`own_store`]), so the order every save keeps (the secret first,
//!   then the file) does not depend on the main loop. The file's mutations are
//!   locked read-modify-writes (`moniker_core::store`), so the worker's
//!   handles and other processes (a sync, `moniker-server`) never lose each
//!   other's changes; the callback reloads the main thread's snapshot
//!   (`AppContext::profiles_changed`).
//! * **The profile file is written there too** ([`write_profiles`]), also by
//!   saves that do not touch the keyring (a field of the profile page, a
//!   startup sequence; issue #7). A sync holds the file's lock while it waits
//!   for the keyring (an unlock prompt), and a save waits for that lock: on the
//!   worker it delays the save, not the window. Saves keep their place in line
//!   with the keyring's changes, so a later edit never loses to an earlier one.
//!   The main thread's handle cannot write (`context::ProfileSnapshot`).
//! * **The app waits for a change under way** ([`change`], not [`read`]). It is
//!   held until the callback has run, so closing the last window does not cut
//!   a save short between the keyring and the file. A read stuck at an unlock
//!   prompt does not keep a windowless process alive.
//! * **A callback still says what went wrong when the page that asked has
//!   gone** (the person moved on while the keyring waited): its message goes
//!   to the window's notifications instead.
//!
//! The values a call carries (a typed password) are `Zeroizing`, moved into
//! the call and wiped when it drops; nothing is logged.

use std::cell::{Cell, RefCell};
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::{Arc, mpsc};
use std::thread;

use gtk::prelude::*;
use gtk::{gio, glib};
use gtk4 as gtk;
use moniker_core::secrets::SecretStore;
use moniker_core::store::ProfileStore;

use crate::context::AppContext;

/// Why a keyring call did not come back (the worker could not be started, or
/// the call panicked), to end a message such as "Could not save the sync key:
/// …".
pub(crate) const STOPPED: &str = "the keyring worker stopped unexpectedly";

/// One call, as the worker runs it.
type Job = Box<dyn FnOnce() + Send>;

thread_local! {
    /// Where calls go: the worker's queue, once it is started. GTK runs on one
    /// thread, so every call is queued from it.
    static WORKER: RefCell<Option<mpsc::Sender<Job>>> = const { RefCell::new(None) };
    /// Calls asked for whose callback has not run yet.
    static PENDING: Cell<usize> = const { Cell::new(0) };
}

/// Run `work`, which only reads the keyring, with the app's secret store on
/// the keyring worker (module docs), then `done` on the main thread with what
/// it returned, or with `None` if it did not come back (the worker could not
/// be started, or `work` panicked; [`STOPPED`] says so). Never blocks the
/// caller.
///
/// `done` runs from the main loop, never inside this call, even when the work
/// could not be queued. Must be called from the thread that runs the main loop.
pub(crate) fn read<R: Send + 'static>(
    ctx: &AppContext,
    work: impl FnOnce(&dyn SecretStore) -> R + Send + 'static,
    done: impl FnOnce(Option<R>) + 'static,
) {
    run(ctx, work, done, false);
}

/// [`read`] for `work` that changes the keyring (and perhaps the profile file):
/// the application is held until `done` has run.
pub(crate) fn change<R: Send + 'static>(
    ctx: &AppContext,
    work: impl FnOnce(&dyn SecretStore) -> R + Send + 'static,
    done: impl FnOnce(Option<R>) + 'static,
) {
    run(ctx, work, done, true);
}

fn run<R: Send + 'static>(
    ctx: &AppContext,
    work: impl FnOnce(&dyn SecretStore) -> R + Send + 'static,
    done: impl FnOnce(Option<R>) + 'static,
    hold: bool,
) {
    let secrets = Arc::clone(&ctx.secrets);
    let (sender, received) = async_channel::bounded::<R>(1);
    let job: Job = Box::new(move || {
        // A closed channel means the main loop is gone.
        let _ = sender.send_blocking(work(secrets.as_ref()));
    });
    // Dropped once `done` has run (module docs).
    let hold = gio::Application::default()
        .filter(|_| hold)
        .map(|app| app.hold());
    PENDING.set(PENDING.get() + 1);
    // A job that cannot be queued is dropped here, with its sender, so the
    // receive below ends at once with nothing.
    drop(submit(job));
    glib::MainContext::default().spawn_local(async move {
        let result = received.recv().await.ok();
        done(result);
        PENDING.set(PENDING.get() - 1);
        drop(hold);
    });
}

/// Queue `job` on the worker, starting it if it is not running (or has gone);
/// gives the job back if no worker can be had.
fn submit(job: Job) -> Result<(), Job> {
    WORKER.with_borrow_mut(|worker| {
        let mut job = job;
        // A second try with a fresh worker, should the first have gone.
        for _ in 0..2 {
            if worker.is_none() {
                match start() {
                    Ok(sender) => *worker = Some(sender),
                    Err(_) => return Err(job),
                }
            }
            let Some(sender) = worker.as_ref() else {
                return Err(job);
            };
            match sender.send(job) {
                Ok(()) => return Ok(()),
                Err(mpsc::SendError(back)) => {
                    *worker = None;
                    job = back;
                }
            }
        }
        Err(job)
    })
}

/// Start the worker: a thread that runs the jobs it is sent, in order, for as
/// long as the main thread can send any. A job that panics ends alone, not the
/// worker, and its callback gets `None`.
fn start() -> std::io::Result<mpsc::Sender<Job>> {
    let (sender, jobs) = mpsc::channel::<Job>();
    thread::Builder::new()
        .name("moniker-keyring".to_owned())
        .spawn(move || {
            for job in jobs {
                let _ = panic::catch_unwind(AssertUnwindSafe(job));
            }
        })?;
    Ok(sender)
}

/// [`change`] for `work` that writes the profile file and does not touch the
/// keyring (module docs): it gets a handle of its own on the file
/// ([`own_store`]). `done` gets what it returned, or a message ending in
/// [`STOPPED`] if it did not come back.
pub(crate) fn write_profiles<R: Send + 'static>(
    ctx: &AppContext,
    work: impl FnOnce(&RefCell<ProfileStore>) -> Result<R, String> + Send + 'static,
    done: impl FnOnce(Result<R, String>) + 'static,
) {
    let path = ctx.store.borrow().path().to_path_buf();
    change(
        ctx,
        move |_| work(&own_store(&path)?),
        move |result| {
            done(result.unwrap_or_else(|| Err(format!("Could not save the profile: {STOPPED}."))))
        },
    );
}

/// A handle of its own on the profile file at `path`, for a call that writes
/// it on the worker (module docs). The message never quotes the file.
pub(crate) fn own_store(path: &Path) -> Result<RefCell<ProfileStore>, String> {
    ProfileStore::open(path)
        .map(RefCell::new)
        .map_err(|err| format!("Could not read the profile file: {err}"))
}

/// How many keyring calls have not come back yet, for tests to wait on.
#[cfg(test)]
pub(crate) fn pending() -> usize {
    PENDING.get()
}

/// Keyring calls made on the main thread so far, by any [`MainThreadWatch`].
#[cfg(test)]
static MAIN_THREAD_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Tests: a secret store that counts the calls made on the thread that made it
/// (the main thread, in the window tests), which the GUI never makes (module
/// docs). Every test context's keyring is one (`AppContext::with_parts`); a test
/// seeds and reads the keyring through its own handle on the store inside.
#[cfg(test)]
pub(crate) struct MainThreadWatch {
    inner: Arc<dyn SecretStore>,
    main: thread::ThreadId,
}

#[cfg(test)]
impl MainThreadWatch {
    pub(crate) fn new(inner: Arc<dyn SecretStore>) -> MainThreadWatch {
        MainThreadWatch {
            inner,
            main: thread::current().id(),
        }
    }

    fn note(&self) {
        if thread::current().id() == self.main {
            MAIN_THREAD_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
impl SecretStore for MainThreadWatch {
    fn get(
        &self,
        key: &str,
    ) -> Result<Option<zeroize::Zeroizing<String>>, moniker_core::secrets::SecretError> {
        self.note();
        self.inner.get(key)
    }
    fn set(&self, key: &str, value: &str) -> Result<(), moniker_core::secrets::SecretError> {
        self.note();
        self.inner.set(key, value)
    }
    fn delete(&self, key: &str) -> Result<(), moniker_core::secrets::SecretError> {
        self.note();
        self.inner.delete(key)
    }
    fn probe(&self) -> Result<(), moniker_core::secrets::SecretError> {
        self.note();
        self.inner.probe()
    }
}

/// How many keyring calls any [`MainThreadWatch`] saw on its main thread.
#[cfg(test)]
pub(crate) fn main_thread_calls() -> usize {
    MAIN_THREAD_CALLS.load(std::sync::atomic::Ordering::SeqCst)
}
