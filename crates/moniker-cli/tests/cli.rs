//! End-to-end tests of the `moniker-server` and `moniker-askpass` binaries.
//!
//! Every test runs the binary against a throwaway config directory and aims at a
//! path that stops before the system keyring or `/dev/tty` is reached: a test
//! must never block on a terminal or read the developer's real secrets. As a
//! second line of defense the environment points the keyring at an unreachable
//! D-Bus address and a throwaway service name.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use assert_cmd::Command;
use assert_cmd::cargo::cargo_bin_cmd;
use moniker_core::profile::Profile;
use moniker_core::store::ProfileStore;
use tempfile::TempDir;

/// A config directory in a tempdir and a way to run the binaries against it.
struct Env {
    dir: TempDir,
}

impl Env {
    fn new() -> Self {
        Env {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn store_path(&self) -> PathBuf {
        self.dir.path().join("config/moniker/profiles.json")
    }

    /// Save `profile` through the core store (stamps `updated_at`).
    fn seed(&self, profile: Profile) {
        ProfileStore::open(&self.store_path())
            .unwrap()
            .upsert(profile)
            .unwrap();
    }

    fn run(&self, mut cmd: Command) -> Command {
        cmd.env("HOME", self.dir.path())
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent/moniker")
            .env("MONIKER_KEYRING_SERVICE", "moniker-test-do-not-use")
            .write_stdin("")
            .timeout(Duration::from_secs(30));
        cmd
    }

    fn moniker(&self) -> Command {
        self.run(cargo_bin_cmd!("moniker-server"))
    }

    fn askpass(&self) -> Command {
        self.run(cargo_bin_cmd!("moniker-askpass"))
    }
}

fn stdout_of(cmd: &mut Command) -> String {
    String::from_utf8(cmd.output().unwrap().stdout).unwrap()
}

#[test]
fn profile_list_empty_prints_no_profiles() {
    let env = Env::new();
    env.moniker()
        .args(["profile", "list"])
        .assert()
        .success()
        .stdout("No profiles.\n");
}

#[test]
fn profile_list_prints_saved_profile() {
    let env = Env::new();
    env.seed(Profile::new("web1", "example.com", "admin"));
    env.moniker()
        .args(["profile", "list"])
        .assert()
        .success()
        .stdout("web1  admin@example.com:22\n");
}

#[test]
fn profile_list_prints_one_line_per_profile_in_store_order() {
    let env = Env::new();
    let mut db = Profile::new("db", "db.example.com", "root");
    db.port = 2222;
    env.seed(Profile::new("web1", "example.com", "admin"));
    env.seed(db);
    env.moniker()
        .args(["profile", "list"])
        .assert()
        .success()
        .stdout("db  root@db.example.com:2222\nweb1  admin@example.com:22\n");
}

#[test]
fn profile_list_skips_deleted_profiles() {
    let env = Env::new();
    let gone = Profile::new("gone", "example.com", "admin");
    let id = gone.id.clone();
    env.seed(gone);
    ProfileStore::open(&env.store_path())
        .unwrap()
        .delete(&id)
        .unwrap();
    env.moniker()
        .args(["profile", "list"])
        .assert()
        .success()
        .stdout("No profiles.\n");
}

#[test]
fn profile_list_refuses_a_name_with_control_characters_clearly() {
    // The store no longer accepts such a name (a NUL aborts debug builds of the
    // GUI, an escape sequence acts on terminals), so a hand-edited file is
    // refused as a whole instead of being printed.
    let env = Env::new();
    fs::create_dir_all(env.store_path().parent().unwrap()).unwrap();
    let id = Profile::new("x", "example.com", "admin").id;
    fs::write(
        env.store_path(),
        format!(
            r#"{{"version":1,"profiles":[{{"id":"{id}","name":"we\u001b[31mb\u009b1","host":"example.com","port":22,"username":"admin","key_file":null,"sequences":[],"transcript":false,"updated_at":1,"deleted":false}}]}}"#
        ),
    )
    .unwrap();
    let assert = env.moniker().args(["profile", "list"]).assert().failure();
    let out = assert.get_output();
    assert!(out.stdout.is_empty(), "{:?}", out.stdout);
    assert!(
        !out.stderr.contains(&0x1b),
        "ESC reached stderr: {:?}",
        out.stderr
    );
    let err = String::from_utf8(out.stderr.clone()).unwrap();
    assert!(!err.contains('\u{9b}'), "C1 CSI reached stderr: {err:?}");
    assert!(err.contains("name"), "{err}");
    assert!(err.contains("control characters"), "{err}");
}

#[test]
fn profile_list_with_a_corrupt_store_fails_clearly() {
    let env = Env::new();
    fs::create_dir_all(env.store_path().parent().unwrap()).unwrap();
    fs::write(env.store_path(), "{ not json").unwrap();
    let assert = env.moniker().args(["profile", "list"]).assert().failure();
    let out = assert.get_output();
    assert!(out.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("corrupt"),
        "{:?}",
        out.stderr
    );
}

#[test]
fn profile_list_without_a_config_directory_fails_clearly() {
    let env = Env::new();
    let assert = env
        .moniker()
        .env_remove("HOME")
        .env_remove("XDG_CONFIG_HOME")
        .args(["profile", "list"])
        .assert()
        .failure();
    let out = assert.get_output();
    assert!(out.stdout.is_empty());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("config directory"), "{err}");
}

#[test]
fn help_lists_subcommands() {
    let env = Env::new();
    let help = stdout_of(env.moniker().arg("--help"));
    for sub in ["profile", "sync-key", "sync-switch", "sync-serve", "sync"] {
        assert!(
            help.lines().any(|l| l.trim_start().starts_with(sub)),
            "`{sub}` missing from help:\n{help}"
        );
    }
}

#[test]
fn sync_unknown_profile_fails_clearly() {
    // Seeded so the store is not simply empty.
    let env = Env::new();
    env.seed(Profile::new("web1", "example.com", "admin"));
    let assert = env.moniker().args(["sync", "nosuch"]).assert().failure();
    let out = assert.get_output();
    assert!(out.stdout.is_empty());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no profile named \"nosuch\""), "{err}");
    // The lookup comes first: the keyring was never reached.
    assert!(!err.contains("Secret Service"), "{err}");
    assert!(!err.contains("sync key"), "{err}");
}

#[test]
fn sync_unknown_profile_error_is_sanitized() {
    let env = Env::new();
    let assert = env
        .moniker()
        .args(["sync", "bad\x1b[2Jname"])
        .assert()
        .failure();
    let err = assert.get_output().stderr.clone();
    assert!(!err.contains(&0x1b), "ESC reached stderr: {err:?}");
    assert!(String::from_utf8_lossy(&err).contains("bad?[2Jname"));
}

#[test]
fn sync_with_duplicate_profile_names_fails_clearly() {
    let env = Env::new();
    env.seed(Profile::new("web1", "a.example.com", "admin"));
    env.seed(Profile::new("web1", "b.example.com", "admin"));
    let assert = env.moniker().args(["sync", "web1"]).assert().failure();
    let out = assert.get_output();
    assert!(out.stdout.is_empty());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("web1"), "{err}");
    assert!(err.contains("unique"), "{err}");
    assert!(!err.contains("Secret Service"), "{err}");
}

#[test]
fn sync_names_match_exactly() {
    let env = Env::new();
    env.seed(Profile::new("Web1", "example.com", "admin"));
    let assert = env.moniker().args(["sync", "web1"]).assert().failure();
    let err = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(err.contains("no profile named \"web1\""), "{err}");
}

#[test]
fn profile_list_marks_a_profile_kept_on_this_machine() {
    let env = Env::new();
    env.seed(Profile {
        local_only: true,
        ..Profile::new("Box", "10.0.0.5", "root")
    });
    env.seed(Profile::new("Web", "example.com", "admin"));
    env.moniker()
        .args(["profile", "list"])
        .assert()
        .success()
        .stdout("Box  root@10.0.0.5:22  (this machine only)\nWeb  admin@example.com:22\n");
}

#[test]
fn sync_switch_turns_sync_off_and_on() {
    let env = Env::new();
    env.moniker()
        .args(["sync-switch", "status"])
        .assert()
        .success()
        .stdout("Sync is on.\n");

    env.moniker()
        .args(["sync-switch", "off"])
        .assert()
        .success()
        .stdout("Sync is off. Nothing is sent to or accepted from other machines.\n");
    env.moniker()
        .args(["sync-switch", "status"])
        .assert()
        .success()
        .stdout("Sync is off.\n");
    let file = fs::read_to_string(env.store_path()).unwrap();
    assert!(file.contains("\"sync_off\": true"), "{file}");

    // Standard input is not a terminal here, so nobody can be asked: without
    // `--yes` it stays off, and says why, without ever reaching /dev/tty.
    let assert = env.moniker().args(["sync-switch", "on"]).assert().failure();
    let out = assert.get_output();
    assert!(out.stdout.is_empty());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--yes"), "{err}");
    env.moniker()
        .args(["sync-switch", "status"])
        .assert()
        .success()
        .stdout("Sync is off.\n");

    let text = stdout_of(env.moniker().args(["sync-switch", "on", "--yes"]));
    assert!(
        text.starts_with("At the next sync, every profile on this machine that syncs"),
        "{text}"
    );
    assert!(text.ends_with("Sync is on.\n"), "{text}");
    env.moniker()
        .args(["sync-switch", "status"])
        .assert()
        .success()
        .stdout("Sync is on.\n");
    let file = fs::read_to_string(env.store_path()).unwrap();
    assert!(file.contains("\"version\": 1"), "{file}");
    assert!(!file.contains("sync_off"), "{file}");
}

#[test]
fn sync_switch_status_names_a_setting_that_cannot_be_read() {
    let env = Env::new();
    fs::create_dir_all(env.store_path().parent().unwrap()).unwrap();
    fs::write(env.store_path(), r#"{"version":2,"profiles":[]}"#).unwrap();
    let out = stdout_of(env.moniker().args(["sync-switch", "status"]));
    assert!(out.starts_with("Sync is off: the setting in "), "{out}");
    assert!(out.contains("could not be read"), "{out}");
}

#[test]
fn sync_while_off_stops_before_the_keyring() {
    let env = Env::new();
    env.seed(Profile::new("web1", "example.com", "admin"));
    env.moniker()
        .args(["sync-switch", "off"])
        .assert()
        .success();
    for name in ["web1", "nosuch"] {
        let assert = env.moniker().args(["sync", name]).assert().failure();
        let out = assert.get_output();
        assert!(out.stdout.is_empty());
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            err.trim_end(),
            "moniker-server: sync is turned off on this machine; turn it on with \
             `moniker-server sync-switch on` or on the Sync page",
            "{name}"
        );
    }
}

#[test]
fn sync_serve_while_off_refuses_before_the_keyring() {
    use moniker_core::sync::{KIND_ERROR, KIND_HELLO, read_frame, write_frame};

    let env = Env::new();
    env.moniker()
        .args(["sync-switch", "off"])
        .assert()
        .success();
    let mut hello = Vec::new();
    write_frame(&mut hello, KIND_HELLO, &[1]).unwrap();
    let assert = env
        .moniker()
        .arg("sync-serve")
        .write_stdin(hello)
        .assert()
        .failure();
    let out = assert.get_output();

    // The client is told, in a frame, with the code for "sync is turned off".
    let mut said = std::io::Cursor::new(&out.stdout);
    let (kind, payload) = read_frame(&mut said).unwrap();
    assert_eq!(kind, KIND_ERROR);
    assert_eq!(payload[0], 5);
    assert_eq!(said.position() as usize, out.stdout.len(), "one frame only");

    // Whoever reads this machine's log is told too, and no sync key was asked
    // for (there is no Secret Service here to answer it).
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("sync is turned off on this machine"), "{err}");
    assert!(!err.contains("sync key"), "{err}");
    assert!(!err.contains("Secret Service"), "{err}");
}

#[test]
fn sync_key_set_stdin_rejects_a_short_key_before_the_keyring() {
    let env = Env::new();
    let assert = env
        .moniker()
        .args(["sync-key", "set", "--stdin"])
        .write_stdin("tooshort\n")
        .assert()
        .failure();
    let out = assert.get_output();
    assert!(out.stdout.is_empty());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("at least 12 characters"), "{err}");
    assert!(!err.contains("tooshort"), "{err}");
    // The check comes first: the keyring was never reached.
    assert!(!err.contains("Secret Service"), "{err}");
    assert!(!err.contains("cannot save"), "{err}");
}

#[test]
fn sync_key_set_stdin_rejects_an_empty_key() {
    let env = Env::new();
    let assert = env
        .moniker()
        .args(["sync-key", "set", "--stdin"])
        .write_stdin("\n")
        .assert()
        .failure();
    let err = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(err.contains("must not be empty"), "{err}");
    assert!(!err.contains("Secret Service"), "{err}");
}

#[test]
fn sync_key_set_stdin_takes_a_key_of_exactly_twelve_characters_to_the_keyring() {
    // Twelve characters pass the check, so the command goes on to store the key.
    // There is no Secret Service here (an unreachable bus, an empty runtime
    // directory), so it stops there with the keyring's error: the point is that
    // the length check did not.
    let env = Env::new();
    let assert = env
        .moniker()
        .env("XDG_RUNTIME_DIR", env.dir.path())
        .args(["sync-key", "set", "--stdin"])
        .write_stdin("abcdefghijkl\n")
        .assert()
        .failure();
    let out = assert.get_output();
    assert!(out.stdout.is_empty());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("cannot save the sync key"), "{err}");
    assert!(!err.contains("at least 12"), "{err}");
    assert!(!err.contains("abcdefghijkl"), "{err}");
}

#[test]
fn askpass_without_arguments_exits_1_with_empty_stdout() {
    let env = Env::new();
    let assert = env.askpass().assert().code(1);
    let out = assert.get_output();
    assert!(out.stdout.is_empty());
    assert!(out.stderr.is_empty());
}
