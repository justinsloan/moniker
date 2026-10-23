//! `moniker-askpass`, the helper OpenSSH runs through `SSH_ASKPASS` to get a
//! password, key passphrase or host-key answer.
//!
//! `ssh` runs it once per prompt with the prompt text as its only argument and
//! reads the answer from its stdout. The decisions (answer a login password
//! from the keyring, ask the user anything else) are in
//! `moniker_core::askpass`; this file reads the environment, finds the profile's
//! identity and prints the result.
//!
//! The environment carries only non-secret values: `SSH_ASKPASS_PROMPT` (set by
//! `ssh`), `MONIKER_PROFILE_ID`, `MONIKER_PID` and, for a terminal tab,
//! `MONIKER_ASKPASS_TOKEN` (set by Moniker), and `XDG_RUNTIME_DIR` (where the
//! token's one-time marker goes). With a token the stored password is sent at
//! most once per connection; if no marker can be made the user is asked.
//!
//! The stored password answers only a prompt from the `ssh` that Moniker
//! started (its parent is `MONIKER_PID`; a ProxyJump or ProxyCommand hop's
//! `ssh` is a child of that one), and only one that names the user and host
//! this connection logs in as. Those come from `ssh -G` with the profile's own
//! user, host, port and key file (run from argv, with `--` before the
//! destination, within five seconds, and only for a prompt that could be that
//! login's), which is how `~/.ssh/config` is taken into account: an alias, a
//! `HostName`, a `HostKeyAlias`. If `ssh -G` fails or its answer is ambiguous,
//! the profile's own user and host are expected, as before. `ssh -G` sees no
//! remote command, so a `Match sessiontype` or `Match command` block (OpenSSH
//! 10.0 and later) is judged as for a plain login. The helper logs nothing: no
//! prompt, no answer, no profile data. On failure stdout stays empty and stderr
//! gets one generic line, and the exit status is 1.

use std::io::{self, Read, Write};
use std::panic;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use moniker_cli::store_path;
use moniker_core::askpass::{
    DevTty, OneTime, PID_ENV, TOKEN_ENV, TtyPrompter, answer, claim_marker, could_be_login_prompt,
    effective_identity, parent_in_stat, started_by_moniker,
};
use moniker_core::profile::Profile;
use moniker_core::secrets::{KeyringSecretStore, SecretStore};
use moniker_core::ssh::resolve_command;
use moniker_core::store::ProfileStore;

/// Why no answer was given. Deliberately generic: the prompt can name a key
/// file or host, and the reason must never echo it.
const NO_ANSWER: &str = "could not get an answer for ssh (is there a terminal to ask on?)";

/// How long `ssh -G` may take before the profile's own host is expected
/// instead. It normally answers in milliseconds; only a `Match exec` or host
/// name canonicalization in `~/.ssh/config` can make it slower.
const RESOLVE_LIMIT: Duration = Duration::from_secs(5);

/// The most of `ssh -G`'s output that is read: it prints a few kilobytes.
const RESOLVE_MAX_BYTES: u64 = 256 * 1024;

/// The variables that lead `ssh` to this helper, kept from `ssh -G`.
const HELPER_ENV: [&str; 5] = [
    "SSH_ASKPASS",
    "SSH_ASKPASS_REQUIRE",
    "MONIKER_PROFILE_ID",
    PID_ENV,
    TOKEN_ENV,
];

fn main() -> ExitCode {
    // A panic message would print the panic's payload and location. Nothing here
    // puts a secret in a panic message, but the helper handles secrets, so it
    // prints a fixed line instead of trusting every dependency.
    panic::set_hook(Box::new(|_| {}));
    match panic::catch_unwind(run_from_environment) {
        Ok(code) => code,
        Err(_) => {
            eprintln!("moniker-askpass: internal error");
            ExitCode::FAILURE
        }
    }
}

/// Read the prompt and the environment, answer, and report the exit status.
fn run_from_environment() -> ExitCode {
    // No prompt is a misuse: say nothing, answer nothing.
    let Some(prompt) = std::env::args_os().nth(1) else {
        return ExitCode::FAILURE;
    };
    let prompt = prompt.to_string_lossy();
    let env_prompt = std::env::var("SSH_ASKPASS_PROMPT").ok();
    let profile_id = std::env::var("MONIKER_PROFILE_ID").ok();
    // A token that is set but not UTF-8 still counts as a token (an empty one,
    // which names no marker): it must not pass for "no token", which would
    // lift the one-time limit.
    let token = std::env::var_os(TOKEN_ENV).map(|t| t.into_string().unwrap_or_default());
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    // Only a question can get the stored password (a message or a yes/no
    // question never does), and only one asked by the ssh Moniker started: the
    // parent of this helper's parent, ssh, must be the Moniker process. A
    // ProxyJump or ProxyCommand hop's ssh is a child of the first ssh, so its
    // prompts go to the user whatever host they name.
    let launcher = std::env::var(PID_ENV).ok();
    let asked = Asked {
        prompt_kind: env_prompt.as_deref(),
        profile_id: profile_id.as_deref(),
        launcher: launcher.as_deref(),
        prompting_ssh_parent: prompting_ssh_parent(),
    };
    let identity = expected_identity(&asked, |id| identity_from_store(id, &prompt));
    let expected = identity
        .as_ref()
        .map(|(user, host)| (user.as_str(), host.as_str()));

    let env = Env {
        prompt_kind: env_prompt.as_deref(),
        profile_id: profile_id.as_deref(),
        token: token.as_deref(),
        runtime_dir: runtime_dir.as_deref(),
    };
    let result = respond(
        &prompt,
        &env,
        expected,
        &KeyringSecretStore::new(),
        &mut DevTty,
        &mut io::stdout().lock(),
    );
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(reason) => {
            eprintln!("moniker-askpass: {reason}");
            ExitCode::FAILURE
        }
    }
}

/// What `ssh` and Moniker put in the helper's environment. None of it is
/// secret.
#[derive(Default)]
struct Env<'a> {
    /// `SSH_ASKPASS_PROMPT`, set by `ssh`: `confirm`, `none` or unset.
    prompt_kind: Option<&'a str>,
    /// `MONIKER_PROFILE_ID`: whose password may answer.
    profile_id: Option<&'a str>,
    /// `MONIKER_ASKPASS_TOKEN`: the connection's one-time token. Set for a
    /// terminal tab, not for a sync.
    token: Option<&'a str>,
    /// `XDG_RUNTIME_DIR`, where the token's marker goes.
    runtime_dir: Option<&'a Path>,
}

/// Answer one prompt and print the answer and a newline to `out`.
///
/// With a token the stored password may be used once per connection: only the
/// helper that creates the token's marker sends it. A runtime directory that is
/// unset or not absolute, or a marker that cannot be made, means no marker, and
/// the user is asked (`moniker_core::askpass`). Without a token the marker is
/// not used.
///
/// On `Err` nothing has been written to `out`, except if writing the answer
/// itself failed halfway.
fn respond(
    prompt: &str,
    env: &Env,
    expected: Option<(&str, &str)>,
    secrets: &dyn SecretStore,
    tty: &mut dyn TtyPrompter,
    out: &mut dyn Write,
) -> Result<(), &'static str> {
    let usable_dir = env.runtime_dir.filter(|dir| dir.is_absolute());
    let claim = || match (env.token, usable_dir) {
        (Some(token), Some(dir)) => claim_marker(dir, token).unwrap_or(false),
        _ => false,
    };
    let once = match env.token {
        Some(_) => OneTime::Claim(&claim),
        None => OneTime::NotUsed,
    };
    let reply = answer(
        prompt,
        env.prompt_kind,
        env.profile_id,
        expected,
        once,
        secrets,
        tty,
    )
    .map_err(|_| NO_ANSWER)?;
    out.write_all(reply.as_bytes())
        .and_then(|()| out.write_all(b"\n"))
        .and_then(|()| out.flush())
        .map_err(|_| "could not write the answer to ssh")
}

/// The `(username, host)` of the profile `profile_id` names, read from the
/// profile file. Any failure (no config directory, unreadable store, no such
/// profile) is `None`, which makes the helper ask the user instead.
fn identity_from_store(profile_id: &str, prompt: &str) -> Option<(String, String)> {
    let store = ProfileStore::open(&store_path().ok()?).ok()?;
    identity_of(&store, profile_id, prompt, resolve_with_ssh)
}

/// What decides whether the stored password may be looked for at all.
struct Asked<'a> {
    /// `SSH_ASKPASS_PROMPT`: `confirm`, `none` or unset.
    prompt_kind: Option<&'a str>,
    /// `MONIKER_PROFILE_ID`.
    profile_id: Option<&'a str>,
    /// `MONIKER_PID`: the Moniker process that started `ssh`.
    launcher: Option<&'a str>,
    /// The parent of the `ssh` that runs this helper ([`prompting_ssh_parent`]).
    prompting_ssh_parent: Option<u32>,
}

/// The identity a prompt may be answered for, as `lookup` gives it for the
/// profile, or `None` without calling `lookup`. Only a question (not a message
/// or a yes/no question) asked by the `ssh` Moniker started gets one: its
/// parent must be `MONIKER_PID` (`askpass::started_by_moniker`). A ProxyJump or
/// ProxyCommand hop's `ssh` is a child of the first `ssh`, so its prompts go to
/// the user whatever host they name.
fn expected_identity(
    asked: &Asked,
    lookup: impl FnOnce(&str) -> Option<(String, String)>,
) -> Option<(String, String)> {
    let from_moniker = asked.prompt_kind.is_none()
        && started_by_moniker(asked.launcher, asked.prompting_ssh_parent);
    asked.profile_id.filter(|_| from_moniker).and_then(lookup)
}

/// The parent of the `ssh` that runs this helper (the helper's own parent), or
/// `None` if that `ssh` went while it was read: a new process that took its
/// id must not pass for it, and once `ssh` has gone the helper's parent is
/// another process.
fn prompting_ssh_parent() -> Option<u32> {
    let ssh = std::os::unix::process::parent_id();
    let parent = parent_of(ssh);
    if std::os::unix::process::parent_id() != ssh {
        return None;
    }
    parent
}

/// The identity `ssh` names in a password prompt for the live profile
/// `profile_id` of `store`: the user and host `ssh -G` reports for its
/// connection (`resolve` runs it), so that `~/.ssh/config` is taken into account
/// (a `HostName`, an alias, a `HostKeyAlias`), or, when that fails or is
/// ambiguous (`askpass::effective_identity`), the profile's own user and host
/// as `ssh` prints them: the host lowercased. `None` for no such profile.
///
/// `ssh -G` is not run (the profile's own identity stands) for a `prompt` that
/// cannot be this profile's login prompt (`askpass::could_be_login_prompt`: a
/// key passphrase, a code, another user's), which it would only delay and whose
/// `Match exec` commands it would run again, nor for a key file whose path
/// holds a control character: `ssh -G` prints the path as it is, and a newline
/// in it would add a line of its own to what is read.
fn identity_of(
    store: &ProfileStore,
    profile_id: &str,
    prompt: &str,
    resolve: impl FnOnce(&Profile) -> Option<String>,
) -> Option<(String, String)> {
    let profile = store.get(profile_id)?;
    let own = (profile.username.clone(), profile.host.to_ascii_lowercase());
    let plain_key_file = profile
        .key_file
        .as_ref()
        .is_none_or(|key| !key.to_string_lossy().chars().any(char::is_control));
    if !could_be_login_prompt(prompt, &profile.username) || !plain_key_file {
        return Some(own);
    }
    let resolved = resolve(profile).and_then(|dump| effective_identity(&dump, &profile.username));
    Some(resolved.unwrap_or(own))
}

/// What `ssh -G` prints for the connection to `profile`, if it answers within
/// [`RESOLVE_LIMIT`].
fn resolve_with_ssh(profile: &Profile) -> Option<String> {
    output_within(resolve_process(profile), RESOLVE_LIMIT)
}

/// `ssh -G` for the connection to `p` (`ssh::resolve_command`), run from argv
/// like every `ssh` Moniker starts, without the variables that lead `ssh` to
/// this helper: it never prompts, and if it ever did, that must not come back
/// here.
fn resolve_process(p: &Profile) -> Command {
    let ssh = resolve_command(p);
    let mut command = Command::new(&ssh.program);
    command.args(&ssh.args);
    for name in HELPER_ENV {
        command.env_remove(name);
    }
    command
}

/// What `command` prints, if it exits successfully within `limit` and prints
/// valid UTF-8, at most [`RESOLVE_MAX_BYTES`] of it (more is not trusted: a
/// dump cut short could hide a repeated key). It gets no input and its
/// errors are discarded. A command still running at `limit` is killed; one
/// that exits but leaves a process holding its output open is given up on at
/// `limit` too.
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
        let read = stdout.take(RESOLVE_MAX_BYTES + 1).read_to_end(&mut out);
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
    if out.len() as u64 > RESOLVE_MAX_BYTES {
        return None;
    }
    String::from_utf8(out).ok()
}

/// The parent of process `pid`, from `/proc/<pid>/stat`.
fn parent_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parent_in_stat(&stat)
}

#[cfg(test)]
mod tests {
    use std::io;

    use moniker_core::askpass::marker_dir;
    use moniker_core::profile::Profile;
    use moniker_core::secrets::{MemorySecretStore, password_key};

    use super::*;

    const ID: &str = "11111111-2222-4333-8444-555555555555";

    /// A terminal with one prepared reply that records what it was asked.
    struct FakeTty {
        reply: io::Result<String>,
        asked: Vec<String>,
        notified: Vec<String>,
    }

    impl FakeTty {
        fn replying(reply: &str) -> Self {
            Self::with(Ok(reply.to_owned()))
        }

        fn broken() -> Self {
            Self::with(Err(io::Error::from(io::ErrorKind::NotFound)))
        }

        fn with(reply: io::Result<String>) -> Self {
            FakeTty {
                reply,
                asked: Vec::new(),
                notified: Vec::new(),
            }
        }
    }

    impl TtyPrompter for FakeTty {
        fn ask(&mut self, prompt: &str, _echo: bool) -> io::Result<String> {
            self.asked.push(prompt.to_owned());
            match &self.reply {
                Ok(s) => Ok(s.clone()),
                Err(e) => Err(io::Error::from(e.kind())),
            }
        }

        fn notify(&mut self, msg: &str) -> io::Result<()> {
            self.notified.push(msg.to_owned());
            match &self.reply {
                Ok(_) => Ok(()),
                Err(e) => Err(io::Error::from(e.kind())),
            }
        }
    }

    fn secrets_with_password(pw: &str) -> MemorySecretStore {
        let s = MemorySecretStore::default();
        s.set(&password_key(ID), pw).unwrap();
        s
    }

    #[test]
    fn a_matching_password_prompt_is_answered_from_the_keyring() {
        let secrets = secrets_with_password("hunter2");
        let mut tty = FakeTty::broken();
        let mut out = Vec::new();
        respond(
            "admin@example.com's password: ",
            &Env {
                prompt_kind: None,
                profile_id: Some(ID),
                ..Env::default()
            },
            Some(("admin", "example.com")),
            &secrets,
            &mut tty,
            &mut out,
        )
        .unwrap();
        assert_eq!(out, b"hunter2\n");
        assert!(tty.asked.is_empty(), "the terminal must not be asked");
    }

    #[test]
    fn without_an_identity_the_user_is_asked_and_the_reply_printed() {
        let secrets = secrets_with_password("hunter2");
        let mut tty = FakeTty::replying("typed by the user");
        let mut out = Vec::new();
        respond(
            "admin@example.com's password: ",
            &Env {
                prompt_kind: None,
                profile_id: Some(ID),
                ..Env::default()
            },
            None,
            &secrets,
            &mut tty,
            &mut out,
        )
        .unwrap();
        assert_eq!(out, b"typed by the user\n");
        assert_eq!(tty.asked, ["admin@example.com's password: "]);
    }

    #[test]
    fn a_host_key_question_goes_to_the_user() {
        let secrets = secrets_with_password("hunter2");
        let mut tty = FakeTty::replying("no");
        let mut out = Vec::new();
        respond(
            "Are you sure you want to continue connecting (yes/no)? ",
            &Env {
                prompt_kind: Some("confirm"),
                profile_id: Some(ID),
                ..Env::default()
            },
            Some(("admin", "example.com")),
            &secrets,
            &mut tty,
            &mut out,
        )
        .unwrap();
        assert_eq!(out, b"no\n");
    }

    #[test]
    fn a_message_is_shown_and_answered_with_an_empty_line() {
        let mut tty = FakeTty::replying("");
        let mut out = Vec::new();
        respond(
            "Authenticated to example.com",
            &Env {
                prompt_kind: Some("none"),
                profile_id: None,
                ..Env::default()
            },
            None,
            &MemorySecretStore::default(),
            &mut tty,
            &mut out,
        )
        .unwrap();
        assert_eq!(out, b"\n");
        assert_eq!(tty.notified, ["Authenticated to example.com"]);
    }

    #[test]
    fn a_failure_prints_nothing_and_does_not_repeat_the_prompt() {
        let secrets = MemorySecretStore::default();
        let mut tty = FakeTty::broken();
        let mut out = Vec::new();
        let prompt = "Enter passphrase for key '/home/me/.ssh/id_secret_name': ";
        let reason = respond(
            prompt,
            &Env {
                prompt_kind: None,
                profile_id: None,
                ..Env::default()
            },
            None,
            &secrets,
            &mut tty,
            &mut out,
        )
        .unwrap_err();
        assert!(out.is_empty(), "stdout must stay empty on failure");
        assert!(!reason.contains("id_secret_name"), "{reason}");
        assert!(!reason.contains("passphrase"), "{reason}");
        assert!(!reason.contains('\n'), "one line only: {reason}");
        assert!(!reason.is_empty());
    }

    #[test]
    fn a_stdout_that_cannot_be_written_is_a_failure() {
        struct Closed;
        impl Write for Closed {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::from(io::ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let secrets = secrets_with_password("hunter2");
        let mut tty = FakeTty::broken();
        let reason = respond(
            "admin@example.com's password: ",
            &Env {
                prompt_kind: None,
                profile_id: Some(ID),
                ..Env::default()
            },
            Some(("admin", "example.com")),
            &secrets,
            &mut tty,
            &mut Closed,
        )
        .unwrap_err();
        assert!(!reason.contains("hunter2"), "{reason}");
    }

    // The one-time marker of a terminal tab.

    const TOKEN: &str = "6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b";
    const PROMPT: &str = "admin@example.com's password: ";
    const IDENTITY: Option<(&str, &str)> = Some(("admin", "example.com"));

    /// What the helper prints for `PROMPT` with `env`, the keyring holding
    /// "hunter2" and the user typing "typed", and whether the user was asked.
    fn printed(env: &Env) -> (String, bool) {
        let secrets = secrets_with_password("hunter2");
        let mut tty = FakeTty::replying("typed");
        let mut out = Vec::new();
        respond(PROMPT, env, IDENTITY, &secrets, &mut tty, &mut out).unwrap();
        let asked = !tty.asked.is_empty();
        if asked {
            assert_eq!(tty.asked, [PROMPT]);
        }
        (String::from_utf8(out).unwrap(), asked)
    }

    #[test]
    fn with_a_token_the_stored_password_is_sent_once() {
        let rt = tempfile::tempdir().unwrap();
        let env = Env {
            profile_id: Some(ID),
            token: Some(TOKEN),
            runtime_dir: Some(rt.path()),
            ..Env::default()
        };
        assert_eq!(printed(&env), ("hunter2\n".to_owned(), false));
        // The server asks again: the user types it.
        assert_eq!(printed(&env), ("typed\n".to_owned(), true));
        assert_eq!(printed(&env), ("typed\n".to_owned(), true));
        assert!(marker_dir(rt.path()).join(TOKEN).exists());
    }

    #[test]
    fn without_a_token_the_marker_is_not_used() {
        // A sync: ssh allows one prompt itself.
        let rt = tempfile::tempdir().unwrap();
        let env = Env {
            profile_id: Some(ID),
            runtime_dir: Some(rt.path()),
            ..Env::default()
        };
        assert_eq!(printed(&env), ("hunter2\n".to_owned(), false));
        assert_eq!(printed(&env), ("hunter2\n".to_owned(), false));
        assert!(!rt.path().join("moniker").exists(), "no marker made");
    }

    #[test]
    fn a_token_without_a_usable_runtime_dir_asks_the_user() {
        for runtime_dir in [None, Some(Path::new("")), Some(Path::new("run/user/1000"))] {
            let env = Env {
                profile_id: Some(ID),
                token: Some(TOKEN),
                runtime_dir,
                ..Env::default()
            };
            assert_eq!(
                printed(&env),
                ("typed\n".to_owned(), true),
                "{runtime_dir:?}"
            );
        }
        assert!(
            !Path::new("run").exists(),
            "nothing made in the working directory"
        );
    }

    #[test]
    fn a_token_that_is_not_a_uuid_asks_the_user() {
        let rt = tempfile::tempdir().unwrap();
        for token in ["", "../../x", "x", &TOKEN.to_uppercase()] {
            let env = Env {
                profile_id: Some(ID),
                token: Some(token),
                runtime_dir: Some(rt.path()),
                ..Env::default()
            };
            assert_eq!(printed(&env), ("typed\n".to_owned(), true), "{token:?}");
        }
        assert!(!rt.path().join("moniker").exists());
    }

    fn store_with(p: Profile) -> (tempfile::TempDir, ProfileStore) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = ProfileStore::open(&dir.path().join("profiles.json")).unwrap();
        store.upsert(p).unwrap();
        (dir, store)
    }

    /// A resolver for which `ssh -G` failed.
    fn no_resolution(_: &Profile) -> Option<String> {
        None
    }

    #[test]
    fn identity_is_the_username_and_the_lowercased_host() {
        // Without `ssh -G`'s answer: ssh prints the host name lowercased in its
        // password prompt.
        let mut p = Profile::new("web1", "Example.COM", "Admin");
        p.id = ID.to_owned();
        let (_dir, store) = store_with(p);
        assert_eq!(
            identity_of(&store, ID, "", no_resolution),
            Some(("Admin".to_owned(), "example.com".to_owned()))
        );
    }

    #[test]
    fn identity_of_an_unknown_profile_is_none() {
        let (_dir, store) = store_with(Profile::new("web1", "example.com", "admin"));
        assert_eq!(identity_of(&store, ID, "", no_resolution), None);
        assert_eq!(identity_of(&store, "", "", no_resolution), None);
    }

    #[test]
    fn identity_of_a_deleted_profile_is_none() {
        let mut p = Profile::new("web1", "example.com", "admin");
        p.id = ID.to_owned();
        let (_dir, mut store) = store_with(p);
        store.delete(ID).unwrap();
        assert_eq!(identity_of(&store, ID, "", no_resolution), None);
    }

    #[test]
    fn identity_follows_what_ssh_g_reports() {
        // `~/.ssh/config` made `web` an alias of web1.example.com.
        let mut p = Profile::new("web1", "web", "admin");
        p.id = ID.to_owned();
        p.port = 2222;
        let (_dir, store) = store_with(p);
        let asked = std::cell::RefCell::new(Vec::new());
        let identity = identity_of(
            &store,
            ID,
            "admin@web1.example.com's password: ",
            |profile| {
                asked
                    .borrow_mut()
                    .push((profile.id.clone(), profile.host.clone(), profile.port));
                Some("user admin\nhostname web1.example.com\nport 2222\n".to_owned())
            },
        );
        assert_eq!(
            identity,
            Some(("admin".to_owned(), "web1.example.com".to_owned()))
        );
        assert_eq!(*asked.borrow(), [(ID.to_owned(), "web".to_owned(), 2222)]);
    }

    #[test]
    fn an_unusable_resolution_falls_back_to_the_profiles_own_host() {
        let mut p = Profile::new("web1", "Web.Example.COM", "admin");
        p.id = ID.to_owned();
        let (_dir, store) = store_with(p);
        for dump in [
            "garbage",
            "user someone-else\nhostname other.example.com\n",
            "user admin\nhostname a.example.com\nhostname b.example.com\n",
        ] {
            assert_eq!(
                identity_of(&store, ID, "admin@web.example.com's password: ", |_| {
                    Some(dump.to_owned())
                }),
                Some(("admin".to_owned(), "web.example.com".to_owned())),
                "{dump:?}"
            );
        }
    }

    fn sh(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(script);
        command
    }

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
    fn ssh_g_runs_from_argv_without_the_helpers_variables() {
        let mut p = Profile::new("web1", "web", "admin");
        p.port = 2222;
        let command = resolve_process(&p);
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

    #[test]
    fn the_parent_is_read_from_proc() {
        assert_eq!(
            parent_of(std::process::id()),
            Some(std::os::unix::process::parent_id())
        );
        assert_eq!(parent_of(0), None);
    }

    // ----- whether the identity is looked up at all -----

    /// What the helper is asked, by `ssh` with `prompt_kind`, from a process
    /// whose parent is `parent`, with `launcher` as `MONIKER_PID`.
    fn asked<'a>(
        prompt_kind: Option<&'a str>,
        launcher: Option<&'a str>,
        parent: Option<u32>,
    ) -> Asked<'a> {
        Asked {
            prompt_kind,
            profile_id: Some(ID),
            launcher,
            prompting_ssh_parent: parent,
        }
    }

    #[test]
    fn only_a_question_of_the_ssh_moniker_started_looks_up_the_identity() {
        let lookups = std::cell::Cell::new(0);
        let lookup = || {
            |id: &str| {
                lookups.set(lookups.get() + 1);
                assert_eq!(id, ID);
                Some(("admin".to_owned(), "web1.example.com".to_owned()))
            }
        };
        assert_eq!(
            expected_identity(&asked(None, Some("4242"), Some(4242)), lookup()),
            Some(("admin".to_owned(), "web1.example.com".to_owned()))
        );
        assert_eq!(lookups.get(), 1);
        let mut no_profile = asked(None, Some("4242"), Some(4242));
        no_profile.profile_id = None;
        for (what, asked) in [
            // A ProxyJump or ProxyCommand hop: its ssh's parent is the first ssh.
            ("a hop", asked(None, Some("4242"), Some(4300))),
            ("a message", asked(Some("none"), Some("4242"), Some(4242))),
            (
                "a yes/no question",
                asked(Some("confirm"), Some("4242"), Some(4242)),
            ),
            ("no MONIKER_PID", asked(None, None, Some(4242))),
            ("an orphaned ssh", asked(None, Some("1"), Some(1))),
            ("an unreadable parent", asked(None, Some("4242"), None)),
            ("no profile", no_profile),
        ] {
            assert_eq!(expected_identity(&asked, lookup()), None, "{what}");
        }
        assert_eq!(lookups.get(), 1, "nothing else was looked up");
    }

    #[test]
    fn ssh_g_runs_only_for_a_prompt_that_could_get_the_password() {
        let mut p = Profile::new("web1", "web", "admin");
        p.id = ID.to_owned();
        let (_dir, store) = store_with(p);
        let runs = std::cell::Cell::new(0);
        let resolve = |_: &Profile| {
            runs.set(runs.get() + 1);
            Some("user admin\nhostname web1.example.com\n".to_owned())
        };
        for prompt in [
            "Enter passphrase for key '/home/admin/.ssh/id_ed25519': ",
            "Verification code: ",
            "root@web1.example.com's password: ",
        ] {
            assert_eq!(
                identity_of(&store, ID, prompt, resolve),
                Some(("admin".to_owned(), "web".to_owned())),
                "{prompt:?}"
            );
        }
        assert_eq!(runs.get(), 0, "ssh -G was not run");
        assert_eq!(
            identity_of(&store, ID, "(admin@web1.example.com) Password: ", resolve),
            Some(("admin".to_owned(), "web1.example.com".to_owned()))
        );
        assert_eq!(runs.get(), 1);
    }

    #[test]
    fn a_key_file_with_control_characters_is_not_resolved() {
        // ssh -G prints the path as it is: a newline in it would add a line,
        // `hostkeyalias evil` say, to what is parsed.
        let mut p = Profile::new("web1", "web", "admin");
        p.id = ID.to_owned();
        p.key_file = Some("/home/admin/x\nhostkeyalias evil".into());
        let (_dir, store) = store_with(p);
        let runs = std::cell::Cell::new(0);
        let identity = identity_of(&store, ID, "admin@evil's password: ", |_| {
            runs.set(runs.get() + 1);
            Some("user admin\nhostname web\nhostkeyalias evil\n".to_owned())
        });
        assert_eq!(identity, Some(("admin".to_owned(), "web".to_owned())));
        assert_eq!(runs.get(), 0);
    }

    #[test]
    fn output_beyond_the_limit_is_not_trusted() {
        // A dump cut short could hide a repeated key.
        let print = |n: u64| sh(&format!("head -c {n} /dev/zero | tr '\\0' x"));
        assert_eq!(
            output_within(print(RESOLVE_MAX_BYTES + 1), Duration::from_secs(5)),
            None
        );
        assert_eq!(
            output_within(print(RESOLVE_MAX_BYTES), Duration::from_secs(5)).map(|out| out.len()),
            Some(RESOLVE_MAX_BYTES as usize)
        );
    }

    #[test]
    fn the_prompting_ssh_parent_is_this_process_grandparent() {
        let parent = std::os::unix::process::parent_id();
        assert_eq!(prompting_ssh_parent(), parent_of(parent));
        assert!(prompting_ssh_parent().is_some());
    }
}
