//! The `moniker` program's own command line, run as a process (issue #15): what
//! it prints and exits with when it opens no window. The program decides that
//! before it reads the profile file or starts GTK or the bus, so these runs have
//! no display, no session bus and no home to speak of, and must still work, and
//! must leave nothing behind. A run that would open a window is never made here.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::process::{Command, Output};

/// Run `moniker` with `args` and nothing else: an empty environment (no
/// `DISPLAY`, no bus), a home and XDG folders that do not exist yet, and
/// nothing to read from.
fn run(home: &Path, args: &[OsString]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_moniker"))
        .args(args)
        .env_clear()
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("cfg"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_RUNTIME_DIR", home.join("run"))
        .current_dir(home)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run moniker")
}

fn words(list: &[&str]) -> Vec<OsString> {
    list.iter().map(OsString::from).collect()
}

fn text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).expect("UTF-8 output")
}

/// Nothing was created in `home`: no profile file, no data folder.
fn assert_untouched(home: &Path) {
    let names: Vec<_> = std::fs::read_dir(home)
        .expect("the home folder")
        .map(|entry| entry.expect("an entry").file_name())
        .collect();
    assert!(names.is_empty(), "the run left {names:?} behind");
}

#[test]
fn help_goes_to_stdout_and_opens_nothing() {
    let home = tempfile::tempdir().expect("a home");
    let out = run(home.path(), &words(&["--help"]));
    assert_eq!(out.status.code(), Some(0));
    let stdout = text(&out.stdout);
    assert!(stdout.starts_with("Usage: moniker"), "{stdout:?}");
    for option in ["--working-directory", "--title", "--hold", "--version"] {
        assert!(stdout.contains(option), "{option} in {stdout:?}");
    }
    assert_eq!(text(&out.stderr), "");
    assert_untouched(home.path());
}

#[test]
fn version_goes_to_stdout_and_opens_nothing() {
    let home = tempfile::tempdir().expect("a home");
    let out = run(home.path(), &words(&["--version"]));
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        text(&out.stdout),
        format!("moniker {}\n", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(text(&out.stderr), "");
    assert_untouched(home.path());
}

#[test]
fn a_mistake_is_one_line_on_stderr_with_status_2() {
    let home = tempfile::tempdir().expect("a home");
    for (args, line) in [
        (vec!["--frob"], "moniker: unknown option: --frob\n"),
        (vec!["-e"], "moniker: -e needs a command to run\n"),
        (vec!["--"], "moniker: -- needs a command to run\n"),
        (vec!["-w"], "moniker: -w needs a value\n"),
        (
            vec!["somewhere"],
            "moniker: unexpected argument: somewhere (to run a command, put it after --)\n",
        ),
        // The words the handover uses are not options a person can type.
        (
            vec!["--moniker-dir=/tmp"],
            "moniker: unknown option: --moniker-dir=/tmp\n",
        ),
        (
            vec!["--moniker-hold"],
            "moniker: unknown option: --moniker-hold\n",
        ),
    ] {
        let out = run(home.path(), &words(&args));
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert_eq!(text(&out.stdout), "", "{args:?}");
        assert_eq!(text(&out.stderr), line, "{args:?}");
    }
    let bad = vec![OsString::from("--"), OsString::from_vec(vec![b'a', 0xff])];
    let out = run(home.path(), &bad);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(text(&out.stdout), "");
    assert_eq!(
        text(&out.stderr),
        "moniker: arguments must be valid UTF-8\n"
    );
    assert_untouched(home.path());
}

#[test]
fn a_mistake_cannot_act_on_the_terminal_it_is_printed_in() {
    // The line goes to this process's standard error, often the terminal it
    // was typed in: an escape sequence or a line break in the argument it
    // names comes out as spaces, so there is one line, with nothing to act on.
    let home = tempfile::tempdir().expect("a home");
    for (args, line) in [
        (
            vec!["--frob\x1b]0;x\x07\nrest"],
            "moniker: unknown option: --frob ]0;x  rest\n",
        ),
        (
            vec!["\x1b[2J\x1b]0;x\x07\nrest"],
            "moniker: unexpected argument:  [2J ]0;x  rest (to run a command, put it after --)\n",
        ),
    ] {
        let out = run(home.path(), &words(&args));
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert_eq!(text(&out.stdout), "", "{args:?}");
        assert_eq!(text(&out.stderr), line, "{args:?}");
    }
    // A `-w` folder whose name is not valid UTF-8 is a mistake too: nothing
    // opens (the folder `moniker` was started in is the only one that can be
    // odd, and it is the notification's business, in a window).
    let odd = vec![
        OsString::from("-w"),
        OsString::from_vec(b"/tmp/b\xffd".to_vec()),
    ];
    let out = run(home.path(), &odd);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        text(&out.stderr),
        "moniker: arguments must be valid UTF-8\n"
    );
    assert_untouched(home.path());
}

#[test]
fn help_beats_a_mistake() {
    let home = tempfile::tempdir().expect("a home");
    let out = run(home.path(), &words(&["--frob", "--help"]));
    assert_eq!(out.status.code(), Some(0));
    assert!(text(&out.stdout).starts_with("Usage: moniker"));
    assert_eq!(text(&out.stderr), "");
    assert_untouched(home.path());
}

#[test]
fn a_profile_file_that_cannot_be_read_does_not_stop_the_answer() {
    // The command line is answered before the profile file is opened: a damaged
    // one (which stops a window from opening) changes nothing for `--help`, a
    // mistake or `--version`, and is not touched.
    let home = tempfile::tempdir().expect("a home");
    let folder = home.path().join("cfg").join("moniker");
    std::fs::create_dir_all(&folder).expect("the config folder");
    let file = folder.join("profiles.json");
    std::fs::write(&file, "this is not a profile file").expect("a damaged file");
    for (args, status) in [
        (vec!["--help"], 0),
        (vec!["--version"], 0),
        (vec!["--frob"], 2),
    ] {
        let out = run(home.path(), &words(&args));
        assert_eq!(out.status.code(), Some(status), "{args:?}");
        assert!(
            !text(&out.stderr).contains("profile file"),
            "{args:?}: {:?}",
            text(&out.stderr)
        );
    }
    assert_eq!(
        std::fs::read_to_string(&file).expect("the file"),
        "this is not a profile file"
    );
}
