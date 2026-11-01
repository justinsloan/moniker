//! Scenarios: a pane that connects through its profile's jump host (issue #9).
//! The jump connection (the master of a shared connection) signs in first, in
//! the pane's own terminal, then the target's `ssh` runs on the same terminal
//! through the shared connection, and the pane ends the master when its
//! connection ends. A profile whose jump host is not there, or goes through a
//! jump host of its own, connects nowhere.
//!
//! Every `ssh` is one fake ([`Fake`]) that branches on its arguments, through
//! the test-only program hook of [`set_test_ssh_program`], which the shared
//! connection's control commands (`-O check`, `-O exit`) go through too. It
//! writes down what it did in a folder of its own. A master that signs in
//! leaves a `sleep 600` in the background in its place, as ssh's `-f` does,
//! deaf to the hang-up as ssh's background part is (it leaves the terminal's
//! session); each scenario kills every one of them it started that still runs
//! ([`Fake::end`], also when it fails), and nothing else.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gtk4 as gtk;
use libadwaita as adw;
use moniker_core::askpass::{claim_marker, marker_dir};
use moniker_core::jump::{MISSING_TEXT, chain_text};
use moniker_core::profile::Profile;
use moniker_core::secrets::{MemorySecretStore, SecretStore, password_key};
use moniker_core::store::ProfileStore;
use vte4::Terminal;
use vte4::prelude::*;

use super::main_window::test_app;
use super::windows::main_window;
use super::{
    HeldKeyring, files_in, make_helper, pane, pump_for, pump_until, shown, wait_for_session,
    wait_for_text,
};
use crate::context::AppContext;
use crate::jump_link::JumpLink;
use crate::panes::{self, REC_PREFIX};
use crate::session_file::{Kind, Node, PaneRecord, TabRecord};
use crate::terminal_tab::{
    ASKPASS_MISSING_NOTICE, PaneHolder, close_pane, open_profile, set_test_ssh_program,
    start_profile, terminal_in, terminal_text,
};
use crate::{keyring, session_layout, settings};

/// The fake `ssh` of one scenario and the folder where it writes down what it
/// did:
///
/// * `log`: one line per run: `master <socket>`, `target`, or the control
///   operation and its arguments (`check …`, `exit …`);
/// * `exits`: the socket of each `-O exit`;
/// * `<socket name>.pid`: the background `sleep` that stands for that socket's
///   master, and `sleeps`: every one started.
///
/// It reads: `hold-master` (while it exists a master waits before signing in),
/// `master-status` (how a master ends: 0, the default, signs in and leaves its
/// `sleep`; anything else is a failed sign-in), `master-deaf` (a master
/// ignores the hang-up of a closed pane and signs in all the same),
/// `check-fails` (every `-O check` fails), `hold-check` (while it exists an
/// `-O check` waits before it answers). `-O check` answers, and `-O exit` ends
/// the recorded `sleep`, only while that `sleep` is still this fake's own
/// (started with `MONIKER_FAKE_MASTER` set to its folder), as a real master
/// answers only while it runs.
///
/// A master or a target prints each argument after `ARG:` and the variables
/// Moniker sets for the askpass helper, as `super::fake_ssh` does, then
/// `MASTER-END` (a master) or `TARGET-UP` (a target). A target then reads a
/// line from its terminal and exits with it as its status.
struct Fake {
    program: String,
    state: PathBuf,
}

impl Fake {
    fn new(bin: &Path, name: &str, state: &Path) -> Fake {
        fs::create_dir_all(state).expect("the fake's folder");
        let state_text = state.to_str().expect("a UTF-8 temporary path");
        assert!(!state_text.contains('\''), "a path the script can quote");
        let script = format!(
            r#"#!/bin/sh
state='{state_text}'
socket= op= master= prev=
for a in "$@"; do
    case $prev in
        -S) socket=$a ;;
        -O) op=$a ;;
    esac
    [ "$a" = -M ] && master=yes
    prev=$a
done
name=${{socket##*/}}
# Whether $1 is this fake's own sleep, still running: a pid recorded earlier
# may belong to another process by now. The master answers and ends only
# while it runs, as a real one does.
own_sleep() {{
    [ -n "$1" ] \
        && [ "$(tr '\0' ' ' < "/proc/$1/cmdline" 2>/dev/null)" = 'sleep 600 ' ] \
        && tr '\0' '\n' < "/proc/$1/environ" 2>/dev/null \
            | grep -qxF "MONIKER_FAKE_MASTER=$state"
}}
if [ -n "$op" ]; then
    printf '%s %s\n' "$op" "$*" >> "$state/log"
    pid=$(cat "$state/$name.pid" 2>/dev/null)
    case $op in
        check)
            while [ -e "$state/hold-check" ]; do sleep 0.05; done
            [ -e "$state/check-fails" ] && exit 255
            own_sleep "$pid" || exit 255
            printf 'Master running (pid=%s)\r\n' "$pid" >&2
            exit 0 ;;
        exit)
            own_sleep "$pid" && kill "$pid" 2>/dev/null
            # Written last: once the line is there, this exit is done.
            printf '%s\n' "$socket" >> "$state/exits"
            printf 'Exit request sent.\r\n' >&2
            exit 0 ;;
    esac
    exit 255
fi
for a in "$@"; do printf 'ARG:%s\n' "$a"; done
printf 'SSH_ASKPASS=%s\n' "${{SSH_ASKPASS-unset}}"
printf 'SSH_ASKPASS_REQUIRE=%s\n' "${{SSH_ASKPASS_REQUIRE-unset}}"
printf 'MONIKER_PROFILE_ID=%s\n' "${{MONIKER_PROFILE_ID-unset}}"
printf 'MONIKER_PID=%s\n' "${{MONIKER_PID-unset}}"
printf 'MONIKER_ASKPASS_TOKEN=%s\n' "${{MONIKER_ASKPASS_TOKEN-unset}}"
printf 'PARENT=%s\n' "$PPID"
if [ -n "$master" ]; then
    printf 'master %s\n' "$socket" >> "$state/log"
    [ -e "$state/master-deaf" ] && trap '' HUP
    while [ -e "$state/hold-master" ]; do sleep 0.05; done
    status=$(cat "$state/master-status" 2>/dev/null || echo 0)
    if [ "$status" = 0 ]; then
        (trap '' HUP; export MONIKER_FAKE_MASTER="$state"; exec sleep 600) \
            </dev/null >/dev/null 2>&1 &
        echo "$!" > "$state/$name.pid.new"
        mv "$state/$name.pid.new" "$state/$name.pid"
        echo "$!" >> "$state/sleeps"
    fi
    echo MASTER-END
    exit "$status"
fi
printf 'target\n' >> "$state/log"
echo TARGET-UP
read status
exit "$status"
"#
        );
        let path = bin.join(name);
        fs::write(&path, script).expect("write the fake ssh");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod the fake");
        Fake {
            program: path.to_str().expect("a UTF-8 temporary path").to_owned(),
            state: state.to_path_buf(),
        }
    }

    /// Whether the fake ran at all, for anything.
    fn ran(&self) -> bool {
        self.state.join("log").exists()
    }

    fn lines(&self, file: &str) -> Vec<String> {
        fs::read_to_string(self.state.join(file))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn log(&self) -> Vec<String> {
        self.lines("log")
    }

    /// The sockets each `-O exit` named.
    fn exits(&self) -> Vec<String> {
        self.lines("exits")
    }

    /// Whether a target ran.
    fn ran_a_target(&self) -> bool {
        self.targets() > 0
    }

    /// How many targets ran.
    fn targets(&self) -> usize {
        self.log().iter().filter(|line| *line == "target").count()
    }

    /// Whether the `-O check` of the master at `socket` was run.
    fn checked(&self, socket: &str) -> bool {
        self.log()
            .iter()
            .any(|line| line.starts_with("check ") && line.contains(socket))
    }

    /// Create or remove the flag file `name`.
    fn set(&self, name: &str, on: bool) {
        let path = self.state.join(name);
        if on {
            fs::write(path, "").expect("set the flag");
        } else {
            let _ = fs::remove_file(path);
        }
    }

    /// How masters end from now on.
    fn master_status(&self, status: i32) {
        fs::write(self.state.join("master-status"), status.to_string()).expect("the status");
    }

    /// The `sleep` that stands for the master at `socket`, once it signed in.
    fn pid_of(&self, socket: &str) -> i32 {
        let name = Path::new(socket)
            .file_name()
            .and_then(|n| n.to_str())
            .expect("a socket name");
        let file = self.state.join(format!("{name}.pid"));
        pump_until("the master's background process", || file.exists());
        fs::read_to_string(&file)
            .expect("the pid file")
            .trim()
            .parse()
            .expect("a pid")
    }

    /// Kill every `sleep` this fake started that still sleeps (the scenario
    /// started them: nothing else is touched, [`sleeping`](Self::sleeping)).
    fn end(&self) {
        for pid in self.lines("sleeps") {
            let Ok(pid) = pid.trim().parse::<i32>() else {
                continue;
            };
            if self.sleeping(pid)
                && let Some(pid) = rustix::process::Pid::from_raw(pid)
            {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
            }
        }
    }

    /// Whether `pid` is a `sleep 600` this fake started that still runs: not a
    /// zombie, and not another process that took the id since (another
    /// scenario's `sleep 600` included). The fake starts each with
    /// `MONIKER_FAKE_MASTER` set to its folder, and only such a process counts.
    fn sleeping(&self, pid: i32) -> bool {
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        let state = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.trim_start().chars().next());
        let cmdline = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        let environ = fs::read(format!("/proc/{pid}/environ")).unwrap_or_default();
        let marker = format!("MONIKER_FAKE_MASTER={}", self.state.display());
        cmdline == b"sleep\x00600\x00"
            && !matches!(state, Some('Z' | 'X'))
            && environ.split(|&b| b == 0).any(|v| v == marker.as_bytes())
    }
}

impl Drop for Fake {
    /// A scenario that fails before its end leaves no `sleep` behind either.
    fn drop(&mut self) {
        self.end();
    }
}

/// The jump host of these scenarios.
fn bastion() -> Profile {
    let mut p = Profile::new("bastion", "bastion.example.test", "bob");
    p.port = 2222;
    p
}

/// A profile that goes through `jump`.
fn through(jump: &Profile) -> Profile {
    let mut p = Profile::new("web", "app.example.test", "alice");
    p.jump_host = Some(jump.id.clone());
    p
}

/// A runtime folder of `dir`'s, private to this user as the real one is.
fn runtime(dir: &Path) -> PathBuf {
    let run = dir.join("run");
    fs::create_dir_all(&run).expect("the runtime folder");
    fs::set_permissions(&run, fs::Permissions::from_mode(0o700)).expect("chmod it");
    run
}

/// A context over `profiles`, with `secrets` as the keyring and `run` as the
/// runtime folder, saving transcripts into `dir`'s `transcripts`.
fn ctx_for(
    dir: &Path,
    profiles: &[Profile],
    secrets: Arc<dyn SecretStore>,
    run: Option<&Path>,
) -> Rc<AppContext> {
    fs::create_dir_all(dir).expect("dir");
    ctx_with_helper(dir, profiles, secrets, run, make_helper(dir))
}

/// [`ctx_for`] with `helper` as the path of the askpass helper.
fn ctx_with_helper(
    dir: &Path,
    profiles: &[Profile],
    secrets: Arc<dyn SecretStore>,
    run: Option<&Path>,
    helper: PathBuf,
) -> Rc<AppContext> {
    fs::create_dir_all(dir).expect("dir");
    let mut store = ProfileStore::open(&dir.join("profiles.json")).expect("store");
    for p in profiles {
        store.upsert(p.clone()).expect("save");
    }
    let ctx = AppContext::with_parts(store, secrets, helper);
    let ctx = match run {
        Some(run) => ctx.with_runtime_dir(run.to_path_buf()),
        None => ctx,
    };
    settings::update(&ctx.settings_path, |s| {
        s.transcripts_dir = Some(dir.join("transcripts"));
    })
    .expect("write the settings");
    Rc::new(ctx)
}

/// A keyring with a password stored for each of `profiles`.
fn with_passwords(profiles: &[&Profile]) -> Arc<MemorySecretStore> {
    let secrets = Arc::new(MemorySecretStore::default());
    for p in profiles {
        secrets
            .set(&password_key(&p.id), &format!("pw-{}", p.name))
            .expect("store the password");
    }
    secrets
}

/// Change the profile `id` in the file, as another process does, and have the
/// window read the file again.
fn edit(dir: &Path, ctx: &AppContext, id: &str, change: impl FnOnce(&mut Profile)) {
    let mut store = ProfileStore::open(&dir.join("profiles.json")).expect("the store");
    let mut p = store.get(id).cloned().expect("the profile");
    change(&mut p);
    store.upsert(p).expect("save it");
    ctx.profiles_changed();
}

/// What the master and then the target printed in `terminal`: the lines up to
/// `MASTER-END`, and those after it.
fn stages(terminal: &Terminal) -> (Vec<String>, Vec<String>) {
    let lines = shown(terminal);
    match lines.iter().position(|line| line == "MASTER-END") {
        Some(at) => (lines[..at].to_vec(), lines[at + 1..].to_vec()),
        None => (lines, Vec::new()),
    }
}

/// The argument after `flag` in the `ARG:` lines of `lines`.
fn arg_after(lines: &[String], flag: &str) -> Option<String> {
    let args: Vec<&str> = lines
        .iter()
        .filter_map(|l| l.strip_prefix("ARG:"))
        .collect();
    let at = args.iter().position(|a| *a == flag)?;
    args.get(at + 1).map(|a| (*a).to_owned())
}

/// The value of `NAME=` in `lines`.
fn var(lines: &[String], name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    lines
        .iter()
        .find_map(|l| l.strip_prefix(&prefix))
        .map(str::to_owned)
}

/// A pane connected through its jump host, once its target is up.
struct Connected {
    page: adw::TabPage,
    holder: PaneHolder,
    terminal: Terminal,
    /// The master's socket.
    socket: String,
    /// The pane's shared connection, as it had it once the target was up.
    link: Rc<JumpLink>,
}

/// Connect to `p` in a new tab of `view` as a click does, and wait until its
/// target's `ssh` runs.
fn connect(view: &adw::TabView, ctx: &Rc<AppContext>, p: &Profile) -> Connected {
    open_profile(view, ctx, p, None, false);
    let page = view.selected_page().expect("the new tab");
    let holder = pane(&page);
    connected(page, holder)
}

/// The connection through its jump host in the pane of `holder` (in `page`),
/// once its target's `ssh` runs.
fn connected(page: adw::TabPage, holder: PaneHolder) -> Connected {
    pump_until("the session", || terminal_in(&holder).is_some());
    let terminal = terminal_in(&holder).expect("the session");
    wait_for_text(&terminal, "TARGET-UP");
    let (master, _) = stages(&terminal);
    let socket = arg_after(&master, "-S").expect("the master's socket");
    let link = holder
        .jump()
        .borrow()
        .clone()
        .expect("its shared connection");
    Connected {
        page,
        holder,
        terminal,
        socket,
        link,
    }
}

/// Wait until the pane of `c` has learned its master's process.
fn learned(c: &Connected) {
    pump_until("the master's process to be learned", || {
        c.link.knows_master()
    });
}

/// Wait until `link` is closed and none of its commands is still running, so
/// that nothing of it runs after the scenario.
fn settled(link: &JumpLink) {
    pump_until("the shared connection to settle", || {
        link.is_closed() && link.idle()
    });
}

/// How many `-O exit` named `socket`.
fn exits_for(fake: &Fake, socket: &str) -> usize {
    fake.exits().iter().filter(|s| *s == socket).count()
}

/// A click on a profile whose jump host is a saved profile connects through
/// it: the master signs in first, as Moniker's own child, with the jump host's
/// profile id and a one-time token of its own; then the target's `ssh` runs on
/// the same terminal, as Moniker's own child too, with its own id and token,
/// its `ProxyCommand` going through the master's socket. The socket is in a
/// private folder of the runtime folder. Each one-time marker goes when its
/// `ssh` exits. Closing the pane ends the master through the pidfd learned.
pub(super) fn a_profile_connects_through_its_jump_host(bin: &Path, dir: &Path) {
    let fake = Fake::new(bin, "jh-connect", &dir.join("fake"));
    let bastion = bastion();
    let web = through(&bastion);
    let run = runtime(dir);
    let secrets = with_passwords(&[&bastion, &web]);
    let ctx = ctx_for(dir, &[bastion.clone(), web.clone()], secrets, Some(&run));
    let app = test_app();
    let main = main_window(&app, &ctx);
    set_test_ssh_program(Some(fake.program.clone()));
    let helper = ctx.askpass_path.display().to_string();
    let us = std::process::id().to_string();

    fake.set("hold-master", true);
    open_profile(&main.view, &ctx, &web, None, false);
    let page = main.view.selected_page().expect("the new tab");
    let holder = pane(&page);
    let terminal = wait_for_session(&page);
    wait_for_text(&terminal, "PARENT=");
    let (master, target) = stages(&terminal);
    assert!(target.is_empty(), "the target waits: {target:?}");
    assert!(
        !fake.ran_a_target(),
        "no target before the jump host signed in"
    );
    let socket = arg_after(&master, "-S").expect("the master's socket");
    let jump_dir = run.join("moniker").join("jump");
    let name = Path::new(&socket)
        .strip_prefix(&jump_dir)
        .unwrap_or_else(|_| panic!("{socket} is in {}", jump_dir.display()))
        .to_str()
        .expect("a UTF-8 name")
        .to_owned();
    assert!(
        name.len() == 32
            && name
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "a random name: {name}"
    );
    assert_eq!(
        fs::metadata(&jump_dir)
            .expect("the jump folder")
            .permissions()
            .mode()
            & 0o777,
        0o700,
        "the sockets' folder is private"
    );
    let jump_token = var(&master, "MONIKER_ASKPASS_TOKEN").expect("the jump host's token");
    assert_eq!(
        master,
        [
            "ARG:-p",
            "ARG:2222",
            "ARG:-f",
            "ARG:-N",
            "ARG:-M",
            "ARG:-S",
            &format!("ARG:{socket}"),
            "ARG:-o",
            "ARG:ControlPersist=10",
            "ARG:--",
            "ARG:bob@bastion.example.test",
            &format!("SSH_ASKPASS={helper}"),
            "SSH_ASKPASS_REQUIRE=force",
            &format!("MONIKER_PROFILE_ID={}", bastion.id),
            &format!("MONIKER_PID={us}"),
            &format!("MONIKER_ASKPASS_TOKEN={jump_token}"),
            // The master is Moniker's own child, which the helper checks.
            &format!("PARENT={us}"),
        ]
    );
    // The helper makes the jump host's marker at its password prompt; the
    // marker goes once the master has exited.
    assert!(claim_marker(&run, &jump_token).expect("claim the marker"));
    let jump_marker = marker_dir(&run).join(&jump_token);

    fake.set("hold-master", false);
    wait_for_text(&terminal, "TARGET-UP");
    assert!(!jump_marker.exists(), "the jump host's marker went");
    let (_, target) = stages(&terminal);
    let target_token = var(&target, "MONIKER_ASKPASS_TOKEN").expect("the target's token");
    assert_ne!(target_token, jump_token, "a token of its own");
    let hop = format!(
        "ssh -S {socket} -o ControlMaster=no -o BatchMode=yes -W '[%h]:%p' \
         -p 2222 -- bob@bastion.example.test"
    );
    assert_eq!(
        target,
        [
            "ARG:-p",
            "ARG:22",
            "ARG:-o",
            &format!("ARG:ProxyCommand={hop}"),
            "ARG:-o",
            "ARG:ControlPath=none",
            "ARG:--",
            "ARG:alice@app.example.test",
            &format!("SSH_ASKPASS={helper}"),
            "SSH_ASKPASS_REQUIRE=force",
            &format!("MONIKER_PROFILE_ID={}", web.id),
            &format!("MONIKER_PID={us}"),
            &format!("MONIKER_ASKPASS_TOKEN={target_token}"),
            &format!("PARENT={us}"),
            "TARGET-UP",
        ]
    );
    let text = terminal_text(&terminal);
    assert!(
        !text.contains("pw-bastion") && !text.contains("pw-web"),
        "no password anywhere: {text}"
    );
    assert!(claim_marker(&run, &target_token).expect("claim the marker"));
    let target_marker = marker_dir(&run).join(&target_token);

    // The pane holds its shared connection and learns the master's process.
    let link = holder
        .jump()
        .borrow()
        .clone()
        .expect("its shared connection");
    assert_eq!(link.socket(), Path::new(&socket));
    pump_until("the master's process to be learned", || link.knows_master());
    let master_pid = fake.pid_of(&socket);
    assert!(fake.sleeping(master_pid));
    assert!(target_marker.exists(), "kept while the target runs");

    // Closing the tab hangs up the target and ends the master, through its
    // pidfd: no `-O exit` was needed.
    main.view.close_page(&page);
    pump_until("the master to end", || !fake.sleeping(master_pid));
    pump_until("the target's marker to go", || !target_marker.exists());
    settled(&link);
    assert!(holder.jump().borrow().is_none(), "the pane let go of it");
    assert_eq!(
        fake.exits(),
        Vec::<String>::new(),
        "ended through its pidfd"
    );

    fake.end();
    set_test_ssh_program(None);
    main.window.close();
    pump_for(Duration::from_millis(200));
}

/// The master ends with its pane's connection, however that ends: the pane
/// closed (through the pidfd once learned, else with `-O exit` for its
/// socket), the target exiting, with success or not, and the window closed.
pub(super) fn closing_the_pane_ends_its_shared_connection(bin: &Path, dir: &Path) {
    let fake = Fake::new(bin, "jh-close", &dir.join("fake"));
    let bastion = bastion();
    let web = through(&bastion);
    let run = runtime(dir);
    let ctx = ctx_for(
        dir,
        &[bastion.clone(), web.clone()],
        Arc::new(MemorySecretStore::default()),
        Some(&run),
    );
    let app = test_app();
    let main = main_window(&app, &ctx);
    set_test_ssh_program(Some(fake.program.clone()));

    // Its process learned: the pane closed ends it through its pidfd.
    let c = connect(&main.view, &ctx, &web);
    learned(&c);
    let pid = fake.pid_of(&c.socket);
    main.view.close_page(&c.page);
    pump_until("the master to end", || !fake.sleeping(pid));
    settled(&c.link);
    assert_eq!(exits_for(&fake, &c.socket), 0, "no -O exit was needed");

    // `-O check` fails: the pane closed tells the master with `-O exit`, and
    // only once, also when the failed check comes back after it.
    fake.set("check-fails", true);
    fake.set("hold-check", true);
    let c = connect(&main.view, &ctx, &web);
    pump_until("the master to be asked for its process", || {
        fake.checked(&c.socket)
    });
    let pid = fake.pid_of(&c.socket);
    main.view.close_page(&c.page);
    pump_until("-O exit for its socket", || {
        exits_for(&fake, &c.socket) == 1
    });
    pump_until("the master to end", || !fake.sleeping(pid));
    assert!(!c.link.idle(), "the check is still out");
    fake.set("hold-check", false);
    settled(&c.link);
    assert_eq!(exits_for(&fake, &c.socket), 1, "told once");
    fake.set("check-fails", false);

    // The target exits with success: its pane closes and the master ends.
    let c = connect(&main.view, &ctx, &web);
    learned(&c);
    let pid = fake.pid_of(&c.socket);
    let tabs = main.view.n_pages();
    c.terminal.feed_child(b"0\n");
    pump_until("the pane to close", || main.view.n_pages() == tabs - 1);
    pump_until("the master to end", || !fake.sleeping(pid));
    settled(&c.link);

    // The target fails: its pane stays with the note, and the master ends.
    let c = connect(&main.view, &ctx, &web);
    learned(&c);
    let pid = fake.pid_of(&c.socket);
    c.terminal.feed_child(b"7\n");
    wait_for_text(&c.terminal, "[session ended: status 7]");
    pump_until("the master to end", || !fake.sleeping(pid));
    settled(&c.link);
    assert!(c.holder.jump().borrow().is_none(), "its connection is over");
    assert_eq!(terminal_in(&c.holder).as_ref(), Some(&c.terminal), "kept");
    main.view.close_page(&c.page);

    // The pane closed while the jump host signs in: the master is hung up,
    // and `-O exit` goes to a socket that is not there yet. One that signs in
    // all the same (it ignores the hang-up) is ended through its pidfd once
    // its process is known (the late answer, plan Ruling R2), with no second
    // `-O exit`, and its target is never started.
    fake.set("master-deaf", true);
    fake.set("hold-master", true);
    let targets = fake.targets();
    open_profile(&main.view, &ctx, &web, None, false);
    let page = main.view.selected_page().expect("the new tab");
    let holder = pane(&page);
    pump_until("the session", || terminal_in(&holder).is_some());
    let terminal = terminal_in(&holder).expect("the session");
    wait_for_text(&terminal, "PARENT=");
    let socket = arg_after(&stages(&terminal).0, "-S").expect("its socket");
    let link = holder
        .jump()
        .borrow()
        .clone()
        .expect("its shared connection");
    main.view.close_page(&page);
    assert!(
        link.is_closed(),
        "closing the pane ends its shared connection"
    );
    // Done before the master signs in: it ended nothing.
    pump_until("-O exit for its socket", || exits_for(&fake, &socket) == 1);
    assert!(link.told_to_exit());
    fake.set("hold-master", false);
    let pid = fake.pid_of(&socket);
    pump_until("the master that signed in late to end", || {
        !fake.sleeping(pid)
    });
    settled(&link);
    assert_eq!(exits_for(&fake, &socket), 1, "ended through its pidfd");
    assert_eq!(fake.targets(), targets, "a closed pane reaches no target");
    fake.set("master-deaf", false);

    // The target's `ssh` cannot be started: the shared connection is ended
    // with it. The fake is away meanwhile, and stays away until the link's
    // commands have all come back, so neither `-O check` nor `-O exit` could
    // reach the master: the scenario ends it itself.
    fake.set("hold-master", true);
    open_profile(&main.view, &ctx, &web, None, false);
    let page = main.view.selected_page().expect("the new tab");
    let holder = pane(&page);
    pump_until("the session", || terminal_in(&holder).is_some());
    let terminal = terminal_in(&holder).expect("the session");
    wait_for_text(&terminal, "PARENT=");
    let socket = arg_after(&stages(&terminal).0, "-S").expect("its socket");
    let link = holder
        .jump()
        .borrow()
        .clone()
        .expect("its shared connection");
    let away = PathBuf::from(format!("{}.away", fake.program));
    fs::rename(&fake.program, &away).expect("take the fake away");
    fake.set("hold-master", false);
    wait_for_text(&terminal, "[could not start");
    settled(&link);
    fs::rename(&away, &fake.program).expect("put the fake back");
    assert!(holder.jump().borrow().is_none(), "the pane let go of it");
    assert!(link.told_to_exit(), "and told its master to end");
    let pid = fake.pid_of(&socket);
    fake.end();
    pump_until("the master to end", || !fake.sleeping(pid));
    main.view.close_page(&page);

    // A window closed ends the master of its pane.
    let second = main_window(&app, &ctx);
    let c = connect(&second.view, &ctx, &web);
    learned(&c);
    let pid = fake.pid_of(&c.socket);
    second.window.close();
    pump_until("the master to end", || !fake.sleeping(pid));
    settled(&c.link);

    fake.end();
    set_test_ssh_program(None);
    main.window.close();
    pump_for(Duration::from_millis(200));
}

/// Two panes through the same jump host each have a master of their own, at a
/// socket of their own; closing one pane ends only its own (Review Focus 2).
pub(super) fn two_panes_keep_their_own_shared_connections(bin: &Path, dir: &Path) {
    let fake = Fake::new(bin, "jh-two", &dir.join("fake"));
    let bastion = bastion();
    let web = through(&bastion);
    let run = runtime(dir);
    let ctx = ctx_for(
        dir,
        &[bastion.clone(), web.clone()],
        Arc::new(MemorySecretStore::default()),
        Some(&run),
    );
    let app = test_app();
    let main = main_window(&app, &ctx);
    set_test_ssh_program(Some(fake.program.clone()));

    let first = connect(&main.view, &ctx, &web);
    learned(&first);
    // The second pane: a split of the same tab, connected from its list.
    gtk::prelude::WidgetExt::activate_action(&main.window, "win.split-right", None)
        .expect("win.split-right");
    let layout = panes::layout_of_page(&first.page).expect("its layout");
    let holder = layout.focused_holder().expect("the new pane");
    assert_ne!(holder, first.holder, "a pane of its own");
    start_profile(&holder, &ctx, &web, None, false);
    let second = connected(first.page.clone(), holder);
    learned(&second);
    assert_ne!(first.socket, second.socket, "a socket each");
    let (one, two) = (fake.pid_of(&first.socket), fake.pid_of(&second.socket));
    assert_ne!(one, two, "a master each");

    close_pane(&first.holder);
    pump_until("the first master to end", || !fake.sleeping(one));
    settled(&first.link);
    pump_for(Duration::from_millis(300));
    assert!(fake.sleeping(two), "the other pane's master runs on");
    assert!(
        !second.link.is_closed(),
        "and its shared connection is open"
    );
    assert!(
        !terminal_text(&second.terminal).contains("[session ended"),
        "the other pane's session runs on"
    );
    assert!(fake.exits().is_empty(), "nothing told to exit");

    main.view.close_page(&second.page);
    pump_until("the second master to end", || !fake.sleeping(two));
    settled(&second.link);

    fake.end();
    set_test_ssh_program(None);
    main.window.close();
    pump_for(Duration::from_millis(200));
}

/// A pane moved to a window of its own (its header's Move to New Window) takes
/// its shared connection along: closing the window it left ends nothing of it,
/// and closing the moved pane ends its master.
pub(super) fn a_moved_pane_takes_its_shared_connection_along(bin: &Path, dir: &Path) {
    let fake = Fake::new(bin, "jh-moved", &dir.join("fake"));
    let bastion = bastion();
    let web = through(&bastion);
    let run = runtime(dir);
    let ctx = ctx_for(
        dir,
        &[bastion.clone(), web.clone()],
        Arc::new(MemorySecretStore::default()),
        Some(&run),
    );
    let app = test_app();
    let main = main_window(&app, &ctx);
    set_test_ssh_program(Some(fake.program.clone()));

    let c = connect(&main.view, &ctx, &web);
    learned(&c);
    let pid = fake.pid_of(&c.socket);
    // Split, so that the pane has its header, then out to a window of its own.
    gtk::prelude::WidgetExt::activate_action(&main.window, "win.split-right", None)
        .expect("win.split-right");
    let pane_widget = panes::pane_of(&c.holder).expect("its pane");
    let header = pane_widget.header();
    pump_until("the pane's header", || header.is_visible());
    let windows = app.windows().len();
    gtk::prelude::WidgetExt::activate_action(header, "pane.pane-to-window", None)
        .expect("pane.pane-to-window");
    pump_until("a window of its own", || app.windows().len() == windows + 1);
    let moved = c
        .holder
        .root()
        .and_downcast::<gtk::Window>()
        .expect("the pane's window");
    assert_ne!(
        &moved,
        main.window.upcast_ref::<gtk::Window>(),
        "the pane left"
    );
    assert!(
        c.holder
            .jump()
            .borrow()
            .as_ref()
            .is_some_and(|link| Rc::ptr_eq(link, &c.link)),
        "its shared connection went with it"
    );

    // The window it left closes: the moved pane's connection runs on.
    main.window.close();
    pump_for(Duration::from_millis(300));
    assert!(fake.sleeping(pid), "its master runs on");
    assert!(!c.link.is_closed(), "its shared connection is open");
    assert!(
        !terminal_text(&c.terminal).contains("[session ended"),
        "its session runs on"
    );
    assert!(fake.exits().is_empty(), "nothing told to exit");

    // Closed where it is now, it ends its master.
    close_pane(&c.holder);
    pump_until("the master to end", || !fake.sleeping(pid));
    settled(&c.link);
    pump_until("its window to close with it", || !moved.is_visible());

    fake.end();
    set_test_ssh_program(None);
    pump_for(Duration::from_millis(200));
}

/// The jump host's profile edited while a pane is connected through it: the
/// pane still ends the master it started, at its own socket and with the
/// command built when it connected (Review Focus 5).
pub(super) fn an_edited_jump_host_still_ends_the_master_it_started(bin: &Path, dir: &Path) {
    let fake = Fake::new(bin, "jh-edited", &dir.join("fake"));
    let bastion = bastion();
    let web = through(&bastion);
    let run = runtime(dir);
    let ctx = ctx_for(
        dir,
        &[bastion.clone(), web.clone()],
        Arc::new(MemorySecretStore::default()),
        Some(&run),
    );
    let app = test_app();
    let main = main_window(&app, &ctx);
    set_test_ssh_program(Some(fake.program.clone()));

    // Not learned (`-O check` fails): `-O exit` names the jump host as it was.
    fake.set("check-fails", true);
    let c = connect(&main.view, &ctx, &web);
    pump_until("the master to be asked for its process", || {
        fake.checked(&c.socket)
    });
    let pid = fake.pid_of(&c.socket);
    edit(dir, &ctx, &bastion.id, |p| {
        p.host = "moved.example.test".to_owned();
        p.port = 2200;
    });
    main.view.close_page(&c.page);
    pump_until("-O exit for its socket", || {
        fake.exits().contains(&c.socket)
    });
    pump_until("the master to end", || !fake.sleeping(pid));
    settled(&c.link);
    assert_eq!(exits_for(&fake, &c.socket), 1, "told once");
    let exit = fake
        .log()
        .into_iter()
        .find(|line| line.starts_with("exit ") && line.contains(&c.socket))
        .expect("the -O exit");
    assert!(
        exit.ends_with("-p 2222 -- bob@bastion.example.test") && !exit.contains("moved"),
        "the jump host as it was: {exit}"
    );
    fake.set("check-fails", false);

    // Learned: edited again meanwhile, the master is ended through its pidfd.
    let c = connect(&main.view, &ctx, &web);
    let (master, _) = stages(&c.terminal);
    assert!(master.contains(&"ARG:bob@moved.example.test".to_owned()));
    learned(&c);
    let pid = fake.pid_of(&c.socket);
    edit(dir, &ctx, &bastion.id, |p| {
        p.host = "elsewhere.example.test".to_owned();
    });
    main.view.close_page(&c.page);
    pump_until("the master to end", || !fake.sleeping(pid));
    settled(&c.link);
    assert_eq!(exits_for(&fake, &c.socket), 0, "through its pidfd");

    fake.end();
    set_test_ssh_program(None);
    main.window.close();
    pump_for(Duration::from_millis(200));
}

/// A jump connection that fails (ssh exits with 255, a wrong password say)
/// leaves ssh's message and Moniker's note in the pane, and the target is never
/// tried. Nothing went to the background, so no `-O exit` is run. The pane
/// stays; its transcript is written once.
pub(super) fn a_failed_jump_connection_never_tries_the_target(bin: &Path, dir: &Path) {
    let fake = Fake::new(bin, "jh-failed", &dir.join("fake"));
    let bastion = bastion();
    let web = through(&bastion);
    let run = runtime(dir);
    let ctx = ctx_for(
        dir,
        &[bastion.clone(), web.clone()],
        Arc::new(MemorySecretStore::default()),
        Some(&run),
    );
    let app = test_app();
    let main = main_window(&app, &ctx);
    set_test_ssh_program(Some(fake.program.clone()));
    let transcripts = dir.join("transcripts");

    fake.master_status(255);
    fake.set("hold-master", true);
    open_profile(&main.view, &ctx, &web, None, true);
    let page = main.view.selected_page().expect("the new tab");
    let holder = pane(&page);
    let terminal = wait_for_session(&page);
    wait_for_text(&terminal, "PARENT=");
    let link = holder
        .jump()
        .borrow()
        .clone()
        .expect("its shared connection");
    fake.set("hold-master", false);
    wait_for_text(&terminal, "Could not connect to the jump host bastion.");
    settled(&link);
    pump_for(Duration::from_millis(300));
    assert!(
        !link.told_to_exit(),
        "no -O exit for a master that did not sign in"
    );
    assert!(fake.exits().is_empty() && !fake.log().iter().any(|l| l.starts_with("exit ")));
    assert!(!fake.ran_a_target(), "the target is never tried");
    assert!(!terminal_text(&terminal).contains("TARGET-UP"));
    let lines = shown(&terminal);
    assert_eq!(
        &lines[lines.len() - 2..],
        ["MASTER-END", "Could not connect to the jump host bastion."],
        "ssh's own output, then the note"
    );
    assert_eq!(
        terminal_in(&holder).as_ref(),
        Some(&terminal),
        "the pane stays"
    );
    assert!(holder.jump().borrow().is_none(), "nothing shared is left");
    assert!(!holder.recording().get(), "its transcript is written");
    assert_eq!(files_in(&transcripts).len(), 1, "once");
    let written = fs::read_to_string(&files_in(&transcripts)[0]).expect("the transcript");
    assert!(
        written.contains("ARG:bob@bastion.example.test"),
        "{written}"
    );
    main.view.close_page(&page);
    pump_for(Duration::from_millis(200));
    assert_eq!(files_in(&transcripts).len(), 1, "still once");

    fake.end();
    set_test_ssh_program(None);
    main.window.close();
    pump_for(Duration::from_millis(200));
}

/// A profile whose jump host is not saved here, or goes through a jump host of
/// its own, connects nowhere: the pane says why, nothing runs (not even the
/// jump host), and there is no transcript and no REC.
pub(super) fn a_waiting_profile_connects_nowhere(bin: &Path, dir: &Path) {
    let fake = Fake::new(bin, "jh-waiting", &dir.join("fake"));
    let bastion = bastion();
    let mut lost = through(&bastion);
    lost.name = "lost".to_owned();
    lost.jump_host = Some(Profile::new("gone", "gone.example.test", "x").id);
    let mut inner = Profile::new("inner", "inner.example.test", "carol");
    inner.jump_host = Some(bastion.id.clone());
    let mut chained = through(&inner);
    chained.name = "chained".to_owned();
    let run = runtime(dir);
    let ctx = ctx_for(
        dir,
        &[
            bastion.clone(),
            lost.clone(),
            inner.clone(),
            chained.clone(),
        ],
        with_passwords(&[&bastion, &lost, &inner, &chained]),
        Some(&run),
    );
    let app = test_app();
    let main = main_window(&app, &ctx);
    set_test_ssh_program(Some(fake.program.clone()));
    let transcripts = dir.join("transcripts");

    for (p, note) in [
        (&lost, MISSING_TEXT.to_owned()),
        (&chained, chain_text("inner")),
    ] {
        let asked = keyring::pending();
        open_profile(&main.view, &ctx, p, None, true);
        assert_eq!(keyring::pending(), asked, "the keyring is asked nothing");
        let page = main.view.selected_page().expect("the new tab");
        let holder = pane(&page);
        let terminal = wait_for_session(&page);
        wait_for_text(&terminal, &note);
        assert_eq!(shown(&terminal), [note.as_str()], "only the note");
        assert_eq!(page.title(), p.name, "named after its profile, no REC");
        assert!(!holder.recording().get(), "no REC");
        assert!(holder.jump().borrow().is_none());
        pump_for(Duration::from_millis(300));
        assert_eq!(
            terminal_in(&holder).as_ref(),
            Some(&terminal),
            "the pane stays"
        );
        // Nothing runs, so nothing is kept waiting for an exit once closed.
        let weak = terminal.downgrade();
        drop(terminal);
        main.view.close_page(&page);
        pump_until("the closed pane's terminal to be freed", || {
            weak.upgrade().is_none()
        });
    }
    assert!(!fake.ran(), "nothing ran: {:?}", fake.log());
    assert_eq!(
        files_in(&transcripts),
        Vec::<PathBuf>::new(),
        "no transcript"
    );
    assert!(
        !run.join("moniker").join("jump").exists(),
        "no shared connection was made"
    );

    set_test_ssh_program(None);
    main.window.close();
    pump_for(Duration::from_millis(200));
}

/// The route is looked at again when the keyring's answer is back (Review
/// Focus 1, plan Ruling R4): a jump host deleted, or given a jump host of its
/// own, while the pane waited, connects nowhere; a profile given another jump
/// host meanwhile goes through that one, whose stored password was not asked
/// for, so its prompt is the terminal's. The keyring is asked once, for both
/// passwords.
pub(super) fn a_jump_host_gone_while_the_keyring_waits_connects_nowhere(bin: &Path, dir: &Path) {
    let fake = Fake::new(bin, "jh-held", &dir.join("fake"));
    let first = bastion();
    let mut second = Profile::new("second", "second.example.test", "carol");
    second.port = 2300;
    let third = Profile::new("third", "third.example.test", "dave");
    let fourth = Profile::new("fourth", "fourth.example.test", "erin");
    let web = through(&first);
    let run = runtime(dir);
    let seeded = with_passwords(&[&first, &second, &third, &fourth, &web]);
    let keyring = Arc::new(HeldKeyring::new(seeded));
    let ctx = ctx_for(
        dir,
        &[
            first.clone(),
            second.clone(),
            third.clone(),
            fourth.clone(),
            web.clone(),
        ],
        Arc::clone(&keyring) as _,
        Some(&run),
    );
    let app = test_app();
    let main = main_window(&app, &ctx);
    set_test_ssh_program(Some(fake.program.clone()));
    let calls = keyring::main_thread_calls();
    let current = |ctx: &AppContext| ctx.store.borrow().get(&web.id).cloned().expect("web");
    let wait_while_held = |ctx: &Rc<AppContext>| -> PaneHolder {
        let before = keyring::pending();
        keyring.hold();
        open_profile(&main.view, ctx, &current(ctx), None, false);
        // Read before the main loop runs again: nothing else can come back
        // in between.
        assert_eq!(keyring::pending(), before + 1, "one question for both");
        let holder = pane(&main.view.selected_page().expect("the new tab"));
        pump_for(Duration::from_millis(200));
        assert!(terminal_in(&holder).is_none(), "it waits for the keyring");
        holder
    };
    let answered = |holder: &PaneHolder| -> Terminal {
        keyring.release();
        pump_until("the session", || terminal_in(holder).is_some());
        terminal_in(holder).expect("the session")
    };

    // Deleted meanwhile.
    let holder = wait_while_held(&ctx);
    let mut store = ProfileStore::open(&dir.join("profiles.json")).expect("the store");
    store.delete(&first.id).expect("delete the jump host");
    ctx.profiles_changed();
    let terminal = answered(&holder);
    wait_for_text(&terminal, MISSING_TEXT);
    pump_for(Duration::from_millis(300));
    assert!(!fake.ran(), "nothing ran: {:?}", fake.log());

    // Given a jump host of its own meanwhile.
    edit(dir, &ctx, &web.id, |p| {
        p.jump_host = Some(second.id.clone())
    });
    let holder = wait_while_held(&ctx);
    edit(dir, &ctx, &second.id, |p| {
        p.jump_host = Some(third.id.clone())
    });
    let terminal = answered(&holder);
    wait_for_text(&terminal, &chain_text("second"));
    pump_for(Duration::from_millis(300));
    assert!(!fake.ran(), "nothing ran: {:?}", fake.log());

    // Another jump host meanwhile: through it, its prompt in the terminal.
    edit(dir, &ctx, &web.id, |p| p.jump_host = Some(third.id.clone()));
    let holder = wait_while_held(&ctx);
    edit(dir, &ctx, &web.id, |p| {
        p.jump_host = Some(fourth.id.clone())
    });
    let terminal = answered(&holder);
    wait_for_text(&terminal, "TARGET-UP");
    let (master, target) = stages(&terminal);
    assert!(
        master.contains(&"ARG:erin@fourth.example.test".to_owned()),
        "{master:?}"
    );
    assert_eq!(var(&master, "SSH_ASKPASS").as_deref(), Some("unset"));
    assert_eq!(var(&master, "MONIKER_PROFILE_ID").as_deref(), Some("unset"));
    assert_eq!(
        var(&target, "MONIKER_PROFILE_ID"),
        Some(web.id.clone()),
        "the target's own password was asked for"
    );
    assert_eq!(
        keyring::main_thread_calls(),
        calls,
        "the keyring was called on the main thread"
    );
    // Closed, it ends its master: nothing of it runs after the scenario.
    let link = holder
        .jump()
        .borrow()
        .clone()
        .expect("its shared connection");
    pump_until("the master's process to be learned", || link.knows_master());
    let pid = fake.pid_of(&arg_after(&master, "-S").expect("its socket"));
    close_pane(&holder);
    pump_until("the master to end", || !fake.sleeping(pid));
    settled(&link);

    fake.end();
    set_test_ssh_program(None);
    main.window.close();
    pump_for(Duration::from_millis(200));
}

/// With no private runtime folder for the shared connection's socket (none at
/// all, or one others may write in) the pane says so and connects nowhere,
/// never straight to the target.
pub(super) fn no_private_runtime_folder_connects_nowhere(bin: &Path, dir: &Path) {
    let fake = Fake::new(bin, "jh-no-run", &dir.join("fake"));
    let bastion = bastion();
    let web = through(&bastion);
    let note = "Could not connect through the jump host bastion: \
                there is no private runtime folder for its shared connection.";
    let open = dir.join("open");
    fs::create_dir_all(&open).expect("a folder");
    fs::set_permissions(&open, fs::Permissions::from_mode(0o777)).expect("open it to all");
    for (case, run) in [("none", None), ("open", Some(open.as_path()))] {
        let ctx = ctx_for(
            &dir.join(case),
            &[bastion.clone(), web.clone()],
            with_passwords(&[&bastion, &web]),
            run,
        );
        let app = test_app();
        let main = main_window(&app, &ctx);
        set_test_ssh_program(Some(fake.program.clone()));
        open_profile(&main.view, &ctx, &web, None, false);
        let page = main.view.selected_page().expect("the new tab");
        let terminal = wait_for_session(&page);
        wait_for_text(&terminal, note);
        assert_eq!(shown(&terminal), [note], "{case}");
        pump_for(Duration::from_millis(300));
        assert!(!fake.ran(), "{case}: nothing ran: {:?}", fake.log());
        set_test_ssh_program(None);
        main.window.close();
        pump_for(Duration::from_millis(200));
    }
    assert!(!open.join("moniker").exists(), "nothing made in it");
}

/// The jump host's stored password needs the askpass helper, as the profile's
/// own does: with the helper missing, the notification says so and the jump
/// host's `ssh` is not pointed at it (ssh asks in the terminal), even when the
/// profile itself has no password stored.
pub(super) fn the_jump_host_s_password_needs_the_helper_too(bin: &Path, dir: &Path) {
    let fake = Fake::new(bin, "jh-helper", &dir.join("fake"));
    let bastion = bastion();
    let web = through(&bastion);
    let run = runtime(dir);
    let ctx = ctx_with_helper(
        dir,
        &[bastion.clone(), web.clone()],
        with_passwords(&[&bastion]),
        Some(&run),
        dir.join("no-such-helper"),
    );
    let app = test_app();
    let main = main_window(&app, &ctx);
    set_test_ssh_program(Some(fake.program.clone()));

    let c = connect(&main.view, &ctx, &web);
    assert_eq!(
        ctx.notice.last_text().as_deref(),
        Some(ASKPASS_MISSING_NOTICE)
    );
    let (master, target) = stages(&c.terminal);
    for (who, lines) in [("the jump host", &master), ("the target", &target)] {
        assert_eq!(var(lines, "SSH_ASKPASS").as_deref(), Some("unset"), "{who}");
        assert_eq!(
            var(lines, "MONIKER_ASKPASS_TOKEN").as_deref(),
            Some("unset"),
            "{who}"
        );
    }
    let pid = fake.pid_of(&c.socket);
    main.view.close_page(&c.page);
    pump_until("the master to end", || !fake.sleeping(pid));
    settled(&c.link);

    fake.end();
    set_test_ssh_program(None);
    main.window.close();
    pump_for(Duration::from_millis(200));
}

/// A pane a layout brings back connects through its profile's jump host as a
/// click does; one whose jump host is not saved shows why.
pub(super) fn a_restored_layout_pane_goes_through_its_jump_host(bin: &Path, dir: &Path) {
    let fake = Fake::new(bin, "jh-layout", &dir.join("fake"));
    let bastion = bastion();
    let web = through(&bastion);
    let mut lost = through(&bastion);
    lost.name = "lost".to_owned();
    lost.jump_host = Some(Profile::new("gone", "gone.example.test", "x").id);
    let run = runtime(dir);
    let ctx = ctx_for(
        dir,
        &[bastion.clone(), web.clone(), lost.clone()],
        Arc::new(MemorySecretStore::default()),
        Some(&run),
    );
    let app = test_app();
    let main = main_window(&app, &ctx);
    set_test_ssh_program(Some(fake.program.clone()));
    let tab = |p: &Profile| TabRecord {
        root: Node::Pane(PaneRecord {
            kind: Kind::Profile {
                id: p.id.clone(),
                name: p.name.clone(),
            },
            name: None,
            scale: 1.0,
        }),
        focused: 0,
    };
    let tabs = main.view.n_pages();
    session_layout::restore(&main.view, &ctx, &[tab(&web), tab(&lost)]);
    assert_eq!(main.view.n_pages(), tabs + 2);

    let restored = main.view.nth_page(tabs);
    let holder = pane(&restored);
    let terminal = wait_for_session(&restored);
    wait_for_text(&terminal, "TARGET-UP");
    let (master, target) = stages(&terminal);
    assert!(master.contains(&"ARG:-M".to_owned()), "{master:?}");
    assert!(master.contains(&"ARG:bob@bastion.example.test".to_owned()));
    assert!(target.contains(&"ARG:alice@app.example.test".to_owned()));
    assert!(holder.jump().borrow().is_some());

    let waiting = main.view.nth_page(tabs + 1);
    main.view.set_selected_page(&waiting);
    let terminal = wait_for_session(&waiting);
    wait_for_text(&terminal, MISSING_TEXT);
    assert_eq!(
        fake.log()
            .iter()
            .filter(|l| l.starts_with("master "))
            .count(),
        1
    );

    let link = holder
        .jump()
        .borrow()
        .clone()
        .expect("its shared connection");
    let pid = fake.pid_of(&arg_after(&master, "-S").expect("the socket"));
    main.window.close();
    pump_until("the master to end", || !fake.sleeping(pid));
    settled(&link);
    fake.end();
    set_test_ssh_program(None);
    pump_for(Duration::from_millis(200));
}

/// REC shows from the moment the jump host's sign-in starts until the target's
/// connection ends, and the one transcript holds both: what the jump host's
/// `ssh` printed and what the target's did.
pub(super) fn rec_lasts_from_the_jump_host_to_the_target_s_end(bin: &Path, dir: &Path) {
    let fake = Fake::new(bin, "jh-rec", &dir.join("fake"));
    let bastion = bastion();
    let web = through(&bastion);
    let run = runtime(dir);
    let ctx = ctx_for(
        dir,
        &[bastion.clone(), web.clone()],
        Arc::new(MemorySecretStore::default()),
        Some(&run),
    );
    let app = test_app();
    let main = main_window(&app, &ctx);
    set_test_ssh_program(Some(fake.program.clone()));
    let transcripts = dir.join("transcripts");

    fake.set("hold-master", true);
    open_profile(&main.view, &ctx, &web, None, true);
    let page = main.view.selected_page().expect("the new tab");
    let holder = pane(&page);
    let terminal = wait_for_session(&page);
    wait_for_text(&terminal, "PARENT=");
    assert!(holder.recording().get(), "REC while the jump host signs in");
    assert_eq!(page.title(), format!("{REC_PREFIX}web"));

    fake.set("hold-master", false);
    let c = connected(page.clone(), holder.clone());
    assert!(holder.recording().get(), "REC while the target runs");
    assert_eq!(page.title(), format!("{REC_PREFIX}web"));
    assert_eq!(files_in(&transcripts), Vec::<PathBuf>::new(), "not yet");
    let pid = fake.pid_of(&c.socket);

    terminal.feed_child(b"3\n");
    wait_for_text(&terminal, "[session ended: status 3]");
    pump_until("the master to end", || !fake.sleeping(pid));
    settled(&c.link);
    assert!(!holder.recording().get(), "REC goes with the transcript");
    assert_eq!(page.title(), "web");
    let files = files_in(&transcripts);
    assert_eq!(files.len(), 1, "one transcript");
    let written = fs::read_to_string(&files[0]).expect("the transcript");
    assert!(
        written.contains("ARG:bob@bastion.example.test")
            && written.contains("MASTER-END")
            && written.contains("ARG:alice@app.example.test")
            && written.contains("TARGET-UP"),
        "both stages: {written}"
    );
    main.view.close_page(&page);
    pump_for(Duration::from_millis(200));
    assert_eq!(files_in(&transcripts).len(), 1, "written once");

    fake.end();
    set_test_ssh_program(None);
    main.window.close();
    pump_for(Duration::from_millis(200));
}
