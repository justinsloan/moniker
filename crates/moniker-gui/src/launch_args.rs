//! The command line: which folder the new window's terminal starts in, whether
//! it runs a command instead of the shell, what its tab is called, and whether
//! a command's tab stays when the command ends.
//!
//! Why these options and no others: the desktop starts a default terminal
//! through `xdg-terminal-exec`, which reads the `X-TerminalArg*` keys of the
//! terminal's desktop file (`--working-directory=DIR` for Files' "Open in
//! Terminal", `--` before the command of an app marked `Terminal=true`,
//! `--title=` and `--hold`), and Debian's `x-terminal-emulator` callers use the
//! xterm forms `-e COMMAND ARG...` and `-T TITLE`. Everything after `--` or `-e`
//! belongs to the command, so a command's own options (`-e htop --title x`)
//! are never read as Moniker's.
//!
//! A command is always an argument list; nothing here builds a shell string.
//!
//! This module is pure: no GTK, no environment reads. [`parse`] turns the
//! arguments (without `argv[0]`) into a [`Request`], [`start`] turns a request
//! into a [`Start`] with the folder chosen (the launching process's directory
//! and the home folder come from the caller), so everything runs under
//! `cargo test` without a display.
//!
//! The launching process does all of that itself, first of all ([`local`]): it
//! prints the usage text, the version or a mistake and exits, without reading
//! the profile file or starting GTK or touching the bus; or it turns the
//! request into a [`Start`] and hands that over, as the words [`encode`] makes,
//! to the application, which gives them to the Moniker that is already running
//! (GApplication) or opens the window itself. [`decode`] reads them there.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// What a new window's first tab does, with the folder already chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Start {
    /// The folder the terminal starts in; the home folder if the one asked
    /// for was not a folder.
    pub(crate) dir: PathBuf,
    /// The tab's name, fixed as if renamed; `None` leaves the usual naming.
    pub(crate) title: Option<String>,
    /// The shell, or a command.
    pub(crate) run: Run,
    /// The folder that was asked for but is not a folder (so `dir` is the home
    /// folder); the window says so in a notification.
    pub(crate) dir_refused: Option<PathBuf>,
}

/// What the terminal runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Run {
    /// The login shell.
    Shell,
    /// A program and its arguments, run directly (never through a shell).
    /// `hold` keeps the tab open when the command ends, whatever its status.
    Command { argv: Vec<String>, hold: bool },
}

/// What the command line asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Parsed {
    /// Open a window.
    Open(Request),
    /// Print [`HELP`] and open nothing.
    Help,
    /// Print the version and open nothing.
    Version,
}

/// The options of a launch, as typed: the folder not yet resolved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Request {
    /// `--working-directory`; an empty path counts as none given.
    pub(crate) dir: Option<PathBuf>,
    /// `--title`; an empty title is `None`.
    pub(crate) title: Option<String>,
    /// `--hold`; only a command can be held.
    pub(crate) hold: bool,
    /// The command and its arguments, after `--` or `-e`; never empty.
    pub(crate) command: Option<Vec<String>>,
}

impl Start {
    /// A shell in the home folder with no title: what New Window and a launch
    /// with no arguments open. `home` comes from the caller to keep this
    /// module free of the environment.
    pub(crate) fn home(home: &Path) -> Start {
        Start {
            dir: home.to_path_buf(),
            title: None,
            run: Run::Shell,
            dir_refused: None,
        }
    }
}

/// The usage text for `--help`: one line for each option, plain ASCII.
pub(crate) const HELP: &str = "\
Usage: moniker [OPTION]... [-- COMMAND [ARG]...]
       moniker --help | --version

Options:
  -w, --working-directory=DIR  Start in the folder DIR
  -T, --title=TITLE            Name the tab TITLE
      --hold                   Keep a command's tab open when the command ends
  -e COMMAND [ARG]...          Run COMMAND with its arguments, not the shell
  -- COMMAND [ARG]...          The same as -e
      --help                   Print this text and exit
      --version                Print the version and exit

Everything after -e or -- belongs to the command.
";

/// The options that take their value from the next argument when it is not
/// attached with `=`. (`-w` and `-T` have no attached form.)
fn takes_separate_value(arg: &OsStr) -> bool {
    arg == "--working-directory" || arg == "-w" || arg == "--title" || arg == "-T"
}

/// `--help` or `--version`, if one stands before the command (before `--` or
/// `-e`) and is not the value of another option. It wins over everything else
/// on the line, mistakes included, so `moniker --frob --help` prints the help.
fn asks_for_text(args: &[OsString]) -> Option<Parsed> {
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_os_str();
        if arg == "--" || arg == "-e" {
            return None;
        }
        if arg == "--help" {
            return Some(Parsed::Help);
        }
        if arg == "--version" {
            return Some(Parsed::Version);
        }
        i += if takes_separate_value(arg) { 2 } else { 1 };
    }
    None
}

fn text(arg: &OsStr) -> Result<&str, String> {
    arg.to_str()
        .ok_or_else(|| "arguments must be valid UTF-8".to_string())
}

/// Reads the arguments (without `argv[0]`).
///
/// The forms are `--working-directory=DIR`, `--working-directory DIR` and
/// `-w DIR`; `--title=TITLE`, `--title TITLE` and `-T TITLE`; `--hold`; and
/// `-- COMMAND [ARG]...` or `-e COMMAND [ARG]...`, which take every argument
/// after them. The separate form of an option takes the next argument whatever
/// it looks like. An option given again replaces the earlier one; an empty
/// value is a value (an empty folder is none, an empty title is none).
/// `--help` and `--version` before the command win over everything else.
/// Anything else wrong is one line saying why, for the launching process's
/// stderr.
pub(crate) fn parse(args: &[OsString]) -> Result<Parsed, String> {
    if let Some(asked) = asks_for_text(args) {
        return Ok(asked);
    }
    let mut request = Request::default();
    let mut i = 0;
    while i < args.len() {
        let arg = text(&args[i])?;
        i += 1;
        if arg == "--" || arg == "-e" {
            let command = args[i..]
                .iter()
                .map(|a| text(a).map(str::to_owned))
                .collect::<Result<Vec<String>, String>>()?;
            if command.is_empty() {
                return Err(format!("{arg} needs a command to run"));
            }
            request.command = Some(command);
            break;
        }
        if arg == "--hold" {
            request.hold = true;
            continue;
        }
        // The attached form: only the long options have one.
        let (option, attached) = match arg.split_once('=') {
            Some((option @ ("--working-directory" | "--title"), value)) => (option, Some(value)),
            _ => (arg, None),
        };
        let is_dir = matches!(option, "--working-directory" | "-w");
        let is_title = matches!(option, "--title" | "-T");
        if is_dir || is_title {
            let value = match attached {
                Some(value) => value,
                None => {
                    let next = args
                        .get(i)
                        .ok_or_else(|| format!("{option} needs a value"))?;
                    i += 1;
                    text(next)?
                }
            };
            if is_dir {
                request.dir = Some(PathBuf::from(value));
            } else {
                request.title = (!value.is_empty()).then(|| value.to_owned());
            }
        } else if arg.starts_with('-') && arg != "-" {
            // (`--help` and `--version` were taken above, so they never get
            // here; `--help=1` and the like are unknown.)
            return Err(format!("unknown option: {arg}"));
        } else {
            return Err(format!(
                "unexpected argument: {arg} (to run a command, put it after --)"
            ));
        }
    }
    Ok(Parsed::Open(request))
}

/// Chooses the folder and what to run.
///
/// `cwd` is the launching process's current directory (`None` if unknown) and
/// `home` the home folder. A folder that was asked for and is a folder is
/// used; a relative one is taken from `cwd` (joined without its `.`
/// components). With none asked for (or an empty one), the terminal starts in
/// `cwd`, unless that is `/` (launchers that start everything there), unknown
/// or not a folder here, and then in `home`, with nothing refused. A folder
/// asked for that is not a folder, or is relative with no `cwd` to take it
/// from, gives `home` and is named in `dir_refused` (as an absolute path when
/// it could be made one). `hold` counts only with a command.
pub(crate) fn start(request: Request, cwd: Option<&Path>, home: &Path) -> Start {
    let Request {
        dir,
        title,
        hold,
        command,
    } = request;
    let run = match command {
        Some(argv) if !argv.is_empty() => Run::Command { argv, hold },
        _ => Run::Shell,
    };
    let cwd = cwd.filter(|cwd| !cwd.as_os_str().is_empty());
    let asked = dir.filter(|dir| !dir.as_os_str().is_empty());
    let (dir, dir_refused) = match asked {
        Some(asked) => {
            let chosen = match (asked.is_absolute(), cwd) {
                (true, _) => Some(asked.clone()),
                // `components` drops the `.` of `./nope` (and a doubled or
                // trailing slash) and keeps `..`, so the notification reads
                // "/home/u/proj/nope", not "/home/u/proj/./nope".
                (false, Some(cwd)) => Some(cwd.join(&asked).components().collect::<PathBuf>()),
                (false, None) => None,
            };
            match chosen {
                Some(chosen) if is_folder(&chosen) => (chosen, None),
                Some(chosen) => (home.to_path_buf(), Some(chosen)),
                None => (home.to_path_buf(), Some(asked)),
            }
        }
        // Nothing asked for, so nothing is refused: a launching directory that
        // is `/`, unknown or not a folder on this machine (a launcher in
        // another container, say) starts in home, and silently.
        None => match cwd {
            Some(cwd) if cwd != Path::new("/") && is_folder(cwd) => (cwd.to_path_buf(), None),
            _ => (home.to_path_buf(), None),
        },
    };
    Start {
        dir,
        title,
        run,
        dir_refused,
    }
}

/// Whether `path` is a folder, or a link to one.
fn is_folder(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_dir())
}

/// What the launching process does with its command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Local {
    /// Print `text` (to stdout or stderr) and exit with `status`. Nothing is
    /// opened, and nothing else is touched: no profile file, no GTK, no bus.
    Print {
        status: i32,
        to_stdout: bool,
        text: String,
    },
    /// Open a window: the arguments to give to the application, which hands
    /// them to the running Moniker if there is one (see [`encode`]).
    HandOver(Vec<String>),
}

/// Decides, in the launching process and before anything else starts, what its
/// command line asks for: the usage text, the version, a one-line mistake (all
/// printed there, with the exit status the shell sees) or a window, whose
/// request is made complete here with [`start`] (the launching process's
/// directory is only known to this process) and put into [`encode`]'s words.
/// `cwd` and `home` are as for [`start`].
pub(crate) fn local(args: &[OsString], cwd: Option<&Path>, home: &Path) -> Local {
    match parse(args) {
        Ok(Parsed::Help) => Local::Print {
            status: 0,
            to_stdout: true,
            text: HELP.to_owned(),
        },
        Ok(Parsed::Version) => Local::Print {
            status: 0,
            to_stdout: true,
            text: format!("moniker {}\n", env!("CARGO_PKG_VERSION")),
        },
        Err(why) => Local::Print {
            status: 2,
            to_stdout: false,
            text: format!("moniker: {why}\n"),
        },
        Ok(Parsed::Open(request)) => Local::HandOver(encode(&start(request, cwd, home))),
    }
}

// The words of the handover. GApplication parses the command line itself
// before Moniker's handler sees it, and it drops a `--` in some places
// (`-- htop` arrives as `htop`, `--working-directory=/tmp -- true` as
// `--working-directory=/tmp true`: the very form `xdg-terminal-exec` sends), so
// the user's own arguments cannot be what travels. What travels is one word for
// each field of a [`Start`], every one starting with `--moniker-` and, except
// the flag, carrying its value after an `=`: a word GLib's parser does not know
// is kept as it is, and a value attached to its option cannot be read as
// another argument, whatever it looks like (`--`, `-e`, `--help`, empty).
// [`parse`] refuses every `--moniker-` word, so what a person types is never
// taken for a handover. That is not what limits one: any client of the same
// user on the session bus can send these words, so it is [`decode`] that keeps
// a handover to opening one local terminal (a folder, a title, a command, hold)
// and nothing else.
const DIR: &str = "--moniker-dir=";
const REFUSED: &str = "--moniker-refused=";
const TITLE: &str = "--moniker-title=";
const ARG: &str = "--moniker-arg=";
const HOLD: &str = "--moniker-hold";

/// The arguments that carry a [`Start`] to the Moniker that opens the window,
/// one for each field, in this order: the folder (`--moniker-dir=`), the folder
/// that was refused (`--moniker-refused=`), the title (`--moniker-title=`), each
/// element of a command, in order and exactly (`--moniker-arg=`, an empty one
/// too), and `--moniker-hold`, only with a command.
///
/// A folder that is not valid UTF-8 cannot travel: it is left out, so that the
/// terminal starts in the home folder ([`decode`]), and named, shown lossily, as
/// the folder refused (one already named stays the one named). A command with
/// no program (which [`start`] never makes) is a shell.
pub(crate) fn encode(start: &Start) -> Vec<String> {
    let mut words = Vec::new();
    let dir = start.dir.to_str();
    if let Some(dir) = dir {
        words.push(format!("{DIR}{dir}"));
    }
    let refused = match (&start.dir_refused, dir) {
        (Some(refused), _) => Some(refused.as_path()),
        (None, None) => Some(start.dir.as_path()),
        (None, Some(_)) => None,
    };
    if let Some(refused) = refused {
        words.push(format!("{REFUSED}{}", refused.to_string_lossy()));
    }
    if let Some(title) = &start.title {
        words.push(format!("{TITLE}{title}"));
    }
    if let Run::Command { argv, hold } = &start.run
        && !argv.is_empty()
    {
        words.extend(argv.iter().map(|arg| format!("{ARG}{arg}")));
        if *hold {
            words.push(HOLD.to_owned());
        }
    }
    words
}

/// The inverse of [`encode`], for the Moniker that opens the window. `home` is
/// the folder when none came (an empty list is a plain window in the home
/// folder). An empty folder, title or refused folder counts as none, as in
/// [`parse`]. The fields may come in any order; the command's arguments keep
/// theirs. Anything else is `unexpected handover: <word>`: a word that is not
/// one of these, a field twice, or a hold with no command.
pub(crate) fn decode(args: &[String], home: &Path) -> Result<Start, String> {
    let unexpected = |word: &str| format!("unexpected handover: {word}");
    let mut dir: Option<&str> = None;
    let mut refused: Option<&str> = None;
    let mut title: Option<&str> = None;
    let mut argv: Vec<String> = Vec::new();
    let mut hold = false;
    for word in args {
        if let Some(value) = word.strip_prefix(DIR) {
            if dir.replace(value).is_some() {
                return Err(unexpected(word));
            }
        } else if let Some(value) = word.strip_prefix(REFUSED) {
            if refused.replace(value).is_some() {
                return Err(unexpected(word));
            }
        } else if let Some(value) = word.strip_prefix(TITLE) {
            if title.replace(value).is_some() {
                return Err(unexpected(word));
            }
        } else if let Some(value) = word.strip_prefix(ARG) {
            argv.push(value.to_owned());
        } else if word == HOLD {
            if std::mem::replace(&mut hold, true) {
                return Err(unexpected(word));
            }
        } else {
            return Err(unexpected(word));
        }
    }
    if hold && argv.is_empty() {
        return Err(unexpected(HOLD));
    }
    fn non_empty(value: Option<&str>) -> Option<&str> {
        value.filter(|value| !value.is_empty())
    }
    Ok(Start {
        dir: non_empty(dir).map_or_else(|| home.to_path_buf(), PathBuf::from),
        title: non_empty(title).map(str::to_owned),
        run: if argv.is_empty() {
            Run::Shell
        } else {
            Run::Command { argv, hold }
        },
        dir_refused: non_empty(refused).map(PathBuf::from),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    fn open(list: &[&str]) -> Request {
        match parse(&args(list)) {
            Ok(Parsed::Open(request)) => request,
            other => panic!("{list:?} should open a window, got {other:?}"),
        }
    }

    fn why(list: &[&str]) -> String {
        match parse(&args(list)) {
            Err(why) => why,
            other => panic!("{list:?} should be refused, got {other:?}"),
        }
    }

    fn strings(list: &[&str]) -> Option<Vec<String>> {
        Some(list.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn no_arguments_open_a_plain_window() {
        assert_eq!(open(&[]), Request::default());
    }

    #[test]
    fn the_forms_of_each_option() {
        for form in [
            &["--working-directory=/a"][..],
            &["--working-directory", "/a"],
            &["-w", "/a"],
        ] {
            let request = open(form);
            assert_eq!(request.dir, Some(PathBuf::from("/a")), "{form:?}");
            assert_eq!(request.title, None, "{form:?}");
            assert!(!request.hold, "{form:?}");
            assert_eq!(request.command, None, "{form:?}");
        }
        for form in [&["--title=T"][..], &["--title", "T"], &["-T", "T"]] {
            let request = open(form);
            assert_eq!(request.title.as_deref(), Some("T"), "{form:?}");
            assert_eq!(request.dir, None, "{form:?}");
            assert!(!request.hold, "{form:?}");
            assert_eq!(request.command, None, "{form:?}");
        }
        let request = open(&["--hold"]);
        assert!(request.hold);
        assert_eq!(request.dir, None);
        assert_eq!(request.title, None);
        assert_eq!(request.command, None);

        // All of them together, with a command last.
        let request = open(&[
            "--hold", "-T", "Logs", "-w", "/var/log", "--", "tail", "-f", "x",
        ]);
        assert_eq!(
            request,
            Request {
                dir: Some(PathBuf::from("/var/log")),
                title: Some("Logs".into()),
                hold: true,
                command: strings(&["tail", "-f", "x"]),
            }
        );
    }

    #[test]
    fn a_value_may_look_like_an_option() {
        // The separate form takes whatever follows, as getopt does; the `=`
        // form says the same without any doubt.
        assert_eq!(open(&["-T", "--help"]).title.as_deref(), Some("--help"));
        assert_eq!(open(&["--title", "-e"]).title.as_deref(), Some("-e"));
        assert_eq!(open(&["-w", "--hold"]).dir, Some(PathBuf::from("--hold")));
        assert_eq!(open(&["--title=-e"]).title.as_deref(), Some("-e"));
        // A value with spaces, non-ASCII letters and an equals sign survives.
        let dir = "/home/u/My Files/Ünïcode=1";
        assert_eq!(
            open(&[&format!("--working-directory={dir}")]).dir,
            Some(PathBuf::from(dir))
        );
        assert_eq!(open(&["-w", dir]).dir, Some(PathBuf::from(dir)));
        assert_eq!(open(&["--title=a=b c"]).title.as_deref(), Some("a=b c"));
    }

    #[test]
    fn the_last_of_a_repeated_option_wins() {
        assert_eq!(
            open(&[
                "-w",
                "/a",
                "--working-directory=/b",
                "--working-directory",
                "/c"
            ])
            .dir,
            Some(PathBuf::from("/c"))
        );
        assert_eq!(
            open(&["--title=A", "-T", "B", "--title", "C"])
                .title
                .as_deref(),
            Some("C")
        );
        assert!(open(&["--hold", "--hold"]).hold);
    }

    #[test]
    fn an_empty_value_is_a_value() {
        // An empty folder is kept as given (`start` treats it as none), an
        // empty title is no title; either one replaces an earlier value.
        assert_eq!(open(&["--working-directory="]).dir, Some(PathBuf::new()));
        assert_eq!(open(&["-w", ""]).dir, Some(PathBuf::new()));
        assert_eq!(open(&["--title="]).title, None);
        assert_eq!(open(&["-T", ""]).title, None);
        assert_eq!(open(&["-w", "/a", "-w", ""]).dir, Some(PathBuf::new()));
        assert_eq!(open(&["--title=A", "--title="]).title, None);
    }

    #[test]
    fn everything_after_dash_dash_or_e_is_the_command() {
        let request = open(&["--", "ls", "-w", "x"]);
        assert_eq!(request.command, strings(&["ls", "-w", "x"]));
        assert_eq!(request.title, None);
        assert_eq!(request.dir, None);
        assert!(!request.hold);

        let request = open(&["-e", "htop", "--title", "x"]);
        assert_eq!(request.command, strings(&["htop", "--title", "x"]));
        assert_eq!(request.title, None);

        // The command's own `--`, `-e`, `--hold`, `--help` and empty
        // arguments are the command's.
        let request = open(&["-e", "sh", "-e", "--", "--hold", "--help", "--version", ""]);
        assert_eq!(
            request.command,
            strings(&["sh", "-e", "--", "--hold", "--help", "--version", ""])
        );
        assert!(!request.hold);

        // Options before the command still count.
        let request = open(&["-T", "Top", "-e", "top"]);
        assert_eq!(request.title.as_deref(), Some("Top"));
        assert_eq!(request.command, strings(&["top"]));

        // A single argument is a command with no arguments.
        assert_eq!(open(&["--", "htop"]).command, strings(&["htop"]));
    }

    #[test]
    fn mistakes_are_one_line_errors() {
        assert_eq!(why(&["--frob"]), "unknown option: --frob");
        assert_eq!(why(&["-x"]), "unknown option: -x");
        assert_eq!(why(&["--frob=1"]), "unknown option: --frob=1");
        assert_eq!(why(&["--hold=yes"]), "unknown option: --hold=yes");
        assert_eq!(why(&["-w=/a"]), "unknown option: -w=/a");
        assert_eq!(
            why(&["dir"]),
            "unexpected argument: dir (to run a command, put it after --)"
        );
        assert_eq!(
            why(&["--hold", "ls", "-l"]),
            "unexpected argument: ls (to run a command, put it after --)"
        );
        assert_eq!(why(&["-w"]), "-w needs a value");
        assert_eq!(
            why(&["--working-directory"]),
            "--working-directory needs a value"
        );
        assert_eq!(why(&["-T"]), "-T needs a value");
        assert_eq!(why(&["--hold", "--title"]), "--title needs a value");
        assert_eq!(why(&["-e"]), "-e needs a command to run");
        assert_eq!(why(&["--"]), "-- needs a command to run");
        assert_eq!(why(&["-T", "x", "--"]), "-- needs a command to run");

        let bad = OsString::from_vec(vec![b'a', 0xff, b'b']);
        for list in [
            vec![bad.clone()],
            vec![OsString::from("-w"), bad.clone()],
            vec![OsString::from("--title"), bad.clone()],
            vec![OsString::from("--"), bad.clone()],
            vec![OsString::from("-e"), OsString::from("ls"), bad.clone()],
        ] {
            assert_eq!(
                parse(&list),
                Err("arguments must be valid UTF-8".to_string()),
                "{list:?}"
            );
        }
    }

    #[test]
    fn help_and_version() {
        assert_eq!(parse(&args(&["--help"])), Ok(Parsed::Help));
        assert_eq!(parse(&args(&["--version"])), Ok(Parsed::Version));
        // They win over any other option, valid or not, wherever they stand
        // before the command; the first one wins.
        for list in [
            &["--hold", "--help"][..],
            &["--help", "--hold"],
            &["-w", "/a", "--help", "-T", "t"],
            &["--frob", "--help"],
            &["dir", "--help"],
            &["--help", "--version"],
            &["--help", "--"],
            &["--help", "-e"],
        ] {
            assert_eq!(parse(&args(list)), Ok(Parsed::Help), "{list:?}");
        }
        for list in [
            &["--version", "--help"][..],
            &["-T", "x", "--version"],
            &["--frob", "--version"],
            &["--version", "-w"],
        ] {
            assert_eq!(parse(&args(list)), Ok(Parsed::Version), "{list:?}");
        }

        // After `--` or `-e` they belong to the command.
        assert_eq!(
            open(&["--", "ls", "--help"]).command,
            strings(&["ls", "--help"])
        );
        assert_eq!(
            open(&["-e", "prog", "--version"]).command,
            strings(&["prog", "--version"])
        );
        assert_eq!(open(&["--", "--help"]).command, strings(&["--help"]));
        // A value is not an option.
        assert_eq!(open(&["-T", "--help"]).title.as_deref(), Some("--help"));
        assert_eq!(
            open(&["--working-directory", "--version"]).dir,
            Some(PathBuf::from("--version"))
        );
        // `--help=1` is not `--help`.
        assert_eq!(why(&["--help=1"]), "unknown option: --help=1");
    }

    #[test]
    fn the_usage_text_names_every_option() {
        assert!(HELP.is_ascii());
        assert!(HELP.ends_with('\n'));
        assert!(!HELP.ends_with("\n\n"));
        assert!(HELP.starts_with("Usage: moniker"));
        for line in HELP.lines() {
            assert!(line.len() <= 79, "{line:?}");
        }
        // Each option is on one line of its own.
        for option in [
            "-w, --working-directory=DIR",
            "-T, --title=TITLE",
            "--hold",
            "--help",
            "--version",
            "-e COMMAND",
            "-- COMMAND",
        ] {
            let lines = HELP
                .lines()
                .filter(|line| line.trim_start().starts_with(option))
                .count();
            assert_eq!(lines, 1, "{option} should start exactly one line");
        }
    }

    #[test]
    fn hold_without_a_command_is_a_shell() {
        let request = open(&["--hold"]);
        assert!(request.hold);
        assert_eq!(request.command, None);
        let home = Path::new("/home/u");
        let started = start(request, None, home);
        assert_eq!(started.run, Run::Shell);
        assert_eq!(started.dir, home);
        assert_eq!(started.dir_refused, None);
        // With a command it is held.
        let started = start(open(&["--hold", "--", "true"]), None, home);
        assert_eq!(
            started.run,
            Run::Command {
                argv: vec!["true".into()],
                hold: true
            }
        );
        // And without `--hold` it is not.
        let started = start(open(&["--", "true"]), None, home);
        assert_eq!(
            started.run,
            Run::Command {
                argv: vec!["true".into()],
                hold: false
            }
        );
    }

    #[test]
    fn the_title_and_command_pass_through() {
        let home = Path::new("/home/u");
        let started = start(open(&["-T", "Logs", "-e", "tail", "-f", "x y"]), None, home);
        assert_eq!(started.title.as_deref(), Some("Logs"));
        assert_eq!(
            started.run,
            Run::Command {
                argv: vec!["tail".into(), "-f".into(), "x y".into()],
                hold: false
            }
        );
        assert_eq!(start(Request::default(), None, home).title, None);
        // A request whose command is an empty list is a shell.
        let request = Request {
            command: Some(Vec::new()),
            ..Request::default()
        };
        assert_eq!(start(request, None, home).run, Run::Shell);
    }

    #[test]
    fn home_is_a_plain_shell_in_the_home_folder() {
        let home = Path::new("/home/u");
        assert_eq!(
            Start::home(home),
            Start {
                dir: home.to_path_buf(),
                title: None,
                run: Run::Shell,
                dir_refused: None
            }
        );
        // The same as no arguments and nothing known of where it was launched.
        assert_eq!(start(Request::default(), None, home), Start::home(home));
    }

    #[test]
    fn the_folder_rule() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("c");
        std::fs::create_dir_all(cwd.join("rel")).unwrap();
        let other = tmp.path().join("other");
        std::fs::create_dir(&other).unwrap();
        let home = tmp.path().join("home");
        let at = |dir: Option<&Path>, cwd: Option<&Path>| {
            let request = Request {
                dir: dir.map(Path::to_path_buf),
                ..Request::default()
            };
            start(request, cwd, &home)
        };

        // A relative folder is taken from the launching directory.
        let started = at(Some(Path::new("rel")), Some(&cwd));
        assert_eq!(started.dir, cwd.join("rel"));
        assert_eq!(started.dir_refused, None);
        // An absolute one is not.
        let started = at(Some(&other), Some(&cwd));
        assert_eq!(started.dir, other);
        assert_eq!(started.dir_refused, None);
        // No folder: the launching directory.
        let started = at(None, Some(&cwd));
        assert_eq!(started.dir, cwd);
        assert_eq!(started.dir_refused, None);
        // Unless it is `/` (launchers that start everything there) or unknown.
        let started = at(None, Some(Path::new("/")));
        assert_eq!(started.dir, home);
        assert_eq!(started.dir_refused, None);
        let started = at(None, None);
        assert_eq!(started.dir, home);
        assert_eq!(started.dir_refused, None);
        // An empty folder is no folder.
        let empty = Some(Path::new(""));
        assert_eq!(at(empty, Some(&cwd)).dir, cwd);
        assert_eq!(at(empty, Some(Path::new("/"))).dir, home);
        assert_eq!(at(empty, None).dir, home);
        assert_eq!(at(empty, None).dir_refused, None);
        // A folder asked for is honored even if it is `/`, and a relative one
        // is joined to `/` when that is where the launcher stood.
        assert_eq!(at(Some(Path::new("/")), Some(&cwd)).dir, Path::new("/"));
        assert_eq!(
            at(Some(Path::new("tmp")), Some(Path::new("/"))).dir,
            Path::new("/tmp")
        );
        // A launching directory that is not a folder here (a missing path, or
        // a file) is no place to start: home, and nothing is refused, because
        // the person asked for no folder.
        let gone = tmp.path().join("gone");
        let started = at(None, Some(&gone));
        assert_eq!(started.dir, home);
        assert_eq!(started.dir_refused, None);
        let file = tmp.path().join("a file");
        std::fs::write(&file, "x").unwrap();
        let started = at(None, Some(&file));
        assert_eq!(started.dir, home);
        assert_eq!(started.dir_refused, None);
        assert_eq!(at(empty, Some(&gone)).dir, home);
        assert_eq!(at(empty, Some(&gone)).dir_refused, None);
    }

    #[test]
    fn a_relative_folder_is_joined_without_its_dots() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("c");
        std::fs::create_dir_all(cwd.join("rel")).unwrap();
        std::fs::create_dir(tmp.path().join("other")).unwrap();
        let home = tmp.path().join("home");
        let at = |dir: &str| {
            let request = Request {
                dir: Some(PathBuf::from(dir)),
                ..Request::default()
            };
            start(request, Some(&cwd), &home)
        };

        // Paths compare by their components, which ignore a `.`, so the text
        // is compared: what the notification will show.
        let shown = |path: &Path| path.to_str().unwrap().to_string();
        let c = shown(&cwd);

        // `.` is the launching directory itself.
        let started = at(".");
        assert_eq!(shown(&started.dir), c);
        assert_eq!(started.dir_refused, None);
        let started = at("./rel");
        assert_eq!(shown(&started.dir), format!("{c}/rel"));
        assert_eq!(started.dir_refused, None);
        // A trailing slash goes too.
        assert_eq!(shown(&at("rel/").dir), format!("{c}/rel"));
        // A refusal names the folder without the dot, so the notification
        // reads ".../c/nope".
        let started = at("./nope");
        assert_eq!(started.dir, home);
        assert_eq!(
            shown(&started.dir_refused.expect("refused")),
            format!("{c}/nope")
        );
        // `..` stays: it is a real step (and a link's parent is not the
        // folder above it, so it is not for this module to fold away).
        let started = at("../other");
        assert_eq!(shown(&started.dir), format!("{c}/../other"));
        assert_eq!(started.dir_refused, None);
    }

    #[test]
    fn a_folder_that_is_not_one_starts_at_home() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let cwd = tmp.path().join("c");
        std::fs::create_dir(&cwd).unwrap();
        let file = tmp.path().join("a file");
        std::fs::write(&file, "x").unwrap();
        std::fs::write(cwd.join("plain"), "x").unwrap();
        let at = |dir: &Path, cwd: Option<&Path>| {
            let request = Request {
                dir: Some(dir.to_path_buf()),
                ..Request::default()
            };
            start(request, cwd, &home)
        };

        // A file.
        let started = at(&file, Some(&cwd));
        assert_eq!(started.dir, home);
        assert_eq!(started.dir_refused, Some(file.clone()));
        // A path that does not exist.
        let missing = tmp.path().join("missing");
        let started = at(&missing, None);
        assert_eq!(started.dir, home);
        assert_eq!(started.dir_refused, Some(missing));
        // A relative one that is not a folder: the refusal names the path that
        // was looked at.
        let started = at(Path::new("plain"), Some(&cwd));
        assert_eq!(started.dir, home);
        assert_eq!(started.dir_refused, Some(cwd.join("plain")));
        let started = at(Path::new("nope"), Some(&cwd));
        assert_eq!(started.dir_refused, Some(cwd.join("nope")));
        // A relative one when the launcher's folder is unknown cannot be
        // resolved, so it is refused as typed.
        let started = at(Path::new("rel"), None);
        assert_eq!(started.dir, home);
        assert_eq!(started.dir_refused, Some(PathBuf::from("rel")));
        // A refused folder does not change what runs or the title.
        let request = open(&["-w", file.to_str().unwrap(), "-T", "t", "--", "ls"]);
        let started = start(request, None, &home);
        assert_eq!(started.dir, home);
        assert_eq!(started.dir_refused, Some(file));
        assert_eq!(started.title.as_deref(), Some("t"));
        assert_eq!(
            started.run,
            Run::Command {
                argv: vec!["ls".into()],
                hold: false
            }
        );

        // A link to a folder is a folder; a dangling one is not.
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&cwd, &link).unwrap();
        let started = at(&link, None);
        assert_eq!(started.dir, link);
        assert_eq!(started.dir_refused, None);
        let dangling = tmp.path().join("dangling");
        std::os::unix::fs::symlink(tmp.path().join("gone"), &dangling).unwrap();
        let started = at(&dangling, None);
        assert_eq!(started.dir, home);
        assert_eq!(started.dir_refused, Some(dangling));
    }

    #[test]
    fn a_folder_with_spaces_and_accents_is_kept() {
        // Files hands over folders like "/home/u/My Files/Ünï" (Review Focus 1).
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let folder = tmp.path().join("home/u/My Files/Ünï code");
        std::fs::create_dir_all(&folder).unwrap();
        let text = folder.to_str().unwrap();
        for form in [
            vec![format!("--working-directory={text}")],
            vec!["--working-directory".to_string(), text.to_string()],
            vec!["-w".to_string(), text.to_string()],
        ] {
            let list: Vec<&str> = form.iter().map(String::as_str).collect();
            let started = start(open(&list), None, &home);
            assert_eq!(started.dir, folder, "{list:?}");
            assert_eq!(started.dir_refused, None, "{list:?}");
        }
        // Relative to a launching directory with such a name too.
        let started = start(open(&["-w", "Ünï code"]), folder.parent(), &home);
        assert_eq!(started.dir, folder);
        assert_eq!(started.dir_refused, None);
    }

    // ----- handing the request to the running Moniker -----

    const HOME: &str = "/home/u";

    fn round(start: &Start) -> Start {
        decode(&encode(start), Path::new(HOME)).expect("what encode wrote decodes")
    }

    fn tokens(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn a_start(dir: &str, run: Run) -> Start {
        Start {
            dir: PathBuf::from(dir),
            title: None,
            run,
            dir_refused: None,
        }
    }

    /// Arguments that are only data after `--moniker-arg=`, though they look
    /// like options, option terminators, GLib's own options or shell syntax.
    const AWKWARD: &[&str] = &[
        "prog",
        "--",
        "-e",
        "--help",
        "--help-all",
        "--help-gapplication",
        "--gapplication-service",
        "--version",
        "-h",
        "",
        " ",
        "a b",
        "  leading and trailing  ",
        "$HOME",
        "it's \"quoted\"",
        "x; echo INJECTED",
        "ünï",
        "=",
        "--moniker-arg=x",
        "--moniker-hold",
        "tab\there",
        "line\nbreak",
    ];

    fn awkward_command(hold: bool) -> Run {
        Run::Command {
            argv: AWKWARD.iter().map(|a| a.to_string()).collect(),
            hold,
        }
    }

    #[test]
    fn a_shell_start_round_trips() {
        let plain = a_start("/home/u/My Files/Ünï", Run::Shell);
        assert_eq!(round(&plain), plain);
        assert_eq!(
            round(&Start::home(Path::new(HOME))),
            Start::home(Path::new(HOME))
        );
        let titled = Start {
            title: Some("Logs".into()),
            ..plain.clone()
        };
        assert_eq!(round(&titled), titled);
    }

    #[test]
    fn a_command_with_awkward_arguments_round_trips_in_order() {
        for hold in [false, true] {
            let start = a_start("/tmp", awkward_command(hold));
            let back = round(&start);
            assert_eq!(back, start, "hold {hold}");
            let Run::Command { argv, .. } = back.run else {
                panic!("a command");
            };
            let expected: Vec<String> = AWKWARD.iter().map(|a| a.to_string()).collect();
            assert_eq!(argv, expected, "every argument, none dropped, in order");
        }
        // One argument is a command with no arguments; an empty one stays.
        for argv in [vec!["htop"], vec![""], vec!["-"], vec!["--", "--"]] {
            let start = a_start(
                "/tmp",
                Run::Command {
                    argv: argv.iter().map(|a| a.to_string()).collect(),
                    hold: false,
                },
            );
            assert_eq!(round(&start), start, "{argv:?}");
        }
    }

    #[test]
    fn a_title_that_looks_like_an_option_round_trips() {
        for title in [
            "-- x",
            "-e",
            "--help",
            "--moniker-dir=/x",
            "a=b",
            " ",
            "ünï\ttab",
        ] {
            let start = Start {
                title: Some(title.into()),
                ..a_start("/tmp", awkward_command(true))
            };
            assert_eq!(round(&start), start, "{title:?}");
        }
    }

    #[test]
    fn a_refused_folder_round_trips() {
        let start = Start {
            dir_refused: Some(PathBuf::from("/home/u/My Files/not a folder")),
            ..Start::home(Path::new(HOME))
        };
        assert_eq!(round(&start), start);
        // With the other fields too.
        let start = Start {
            title: Some("t".into()),
            dir_refused: Some(PathBuf::from("rel/ünï")),
            ..a_start(HOME, awkward_command(true))
        };
        assert_eq!(round(&start), start);
    }

    #[test]
    fn the_tokens_have_the_documented_form_and_order() {
        let start = Start {
            dir: PathBuf::from("/home/u"),
            title: Some("-- x".into()),
            run: Run::Command {
                argv: vec!["tail".into(), "-f".into(), "".into(), "a b".into()],
                hold: true,
            },
            dir_refused: Some(PathBuf::from("/nope")),
        };
        assert_eq!(
            encode(&start),
            tokens(&[
                "--moniker-dir=/home/u",
                "--moniker-refused=/nope",
                "--moniker-title=-- x",
                "--moniker-arg=tail",
                "--moniker-arg=-f",
                "--moniker-arg=",
                "--moniker-arg=a b",
                "--moniker-hold",
            ])
        );
        assert_eq!(
            encode(&Start::home(Path::new(HOME))),
            tokens(&["--moniker-dir=/home/u"])
        );
        // A command with nothing to run is a shell, and a shell has nothing to
        // hold (`start` never makes one).
        let held_shell = a_start(
            "/tmp",
            Run::Command {
                argv: Vec::new(),
                hold: true,
            },
        );
        assert_eq!(encode(&held_shell), tokens(&["--moniker-dir=/tmp"]));
        assert_eq!(round(&held_shell).run, Run::Shell);
    }

    #[test]
    fn every_token_is_one_that_glib_leaves_alone() {
        // GLib parses the arguments before Moniker sees them: it drops a `--`
        // in some places and may read an unknown option and the argument after
        // it as a pair. A token that starts with `--moniker-` and is either
        // the flag `--moniker-hold` or holds an `=` (so its value is attached)
        // arrived intact in the builder's probe, and the window suite checks
        // it against GLib itself; this is the property the handover relies on.
        let starts = [
            Start::home(Path::new(HOME)),
            a_start("/tmp", awkward_command(false)),
            Start {
                title: Some("-- x".into()),
                dir_refused: Some(PathBuf::from("--")),
                ..a_start("/tmp", awkward_command(true))
            },
            Start {
                title: Some("".into()),
                ..a_start("--", Run::Shell)
            },
        ];
        for start in &starts {
            let written = encode(start);
            assert!(!written.is_empty());
            for token in written {
                assert!(token.starts_with("--moniker-"), "{token:?}");
                assert!(
                    token == "--moniker-hold" || token.contains('='),
                    "{token:?} is neither the flag nor attached"
                );
            }
        }
    }

    #[test]
    fn decode_refuses_what_it_does_not_know() {
        let home = Path::new(HOME);
        for token in [
            "--moniker-frob=1",
            "--moniker-frob",
            "--moniker-",
            "--moniker-dir",
            "--moniker-holdx",
            "--moniker-hold=1",
            "--moniker-command",
            "--help",
            "--version",
            "-e",
            "--",
            "-w",
            "ls",
            "",
            "moniker-dir=/tmp",
        ] {
            let refused = Err(format!("unexpected handover: {token}"));
            assert_eq!(
                decode(&tokens(&["--moniker-dir=/tmp", token]), home),
                refused,
                "{token:?}"
            );
            // Alone, too.
            assert_eq!(decode(&tokens(&[token]), home), refused, "{token:?}");
        }
    }

    #[test]
    fn decode_refuses_a_field_given_twice_and_a_hold_without_a_command() {
        let home = Path::new(HOME);
        // (the tokens, the one named in the error: the second of the field)
        for (list, second) in [
            (
                &["--moniker-dir=/a", "--moniker-dir=/b"][..],
                "--moniker-dir=/b",
            ),
            (
                &["--moniker-dir=/a", "--moniker-dir=/a"],
                "--moniker-dir=/a",
            ),
            (
                &["--moniker-refused=/a", "--moniker-refused=/b"],
                "--moniker-refused=/b",
            ),
            (
                &["--moniker-title=a", "--moniker-title=b"],
                "--moniker-title=b",
            ),
            (
                &["--moniker-title=", "--moniker-title="],
                "--moniker-title=",
            ),
            (
                &["--moniker-arg=x", "--moniker-hold", "--moniker-hold"],
                "--moniker-hold",
            ),
        ] {
            assert_eq!(
                decode(&tokens(list), home),
                Err(format!("unexpected handover: {second}")),
                "{list:?}"
            );
        }
        // A hold needs a command to hold.
        assert_eq!(
            decode(&tokens(&["--moniker-hold"]), home),
            Err("unexpected handover: --moniker-hold".to_string())
        );
        assert_eq!(
            decode(&tokens(&["--moniker-dir=/tmp", "--moniker-hold"]), home),
            Err("unexpected handover: --moniker-hold".to_string())
        );
        // The argument may stand anywhere: it is still a command.
        assert!(decode(&tokens(&["--moniker-hold", "--moniker-arg=x"]), home).is_ok());
    }

    #[test]
    fn no_arguments_decode_to_the_home_start() {
        let home = Path::new(HOME);
        assert_eq!(decode(&[], home), Ok(Start::home(home)));
        // An empty value is no value: the home folder, no title, nothing
        // refused (`start` never makes one).
        assert_eq!(
            decode(
                &tokens(&["--moniker-dir=", "--moniker-title=", "--moniker-refused="]),
                home
            ),
            Ok(Start::home(home))
        );
        // With no folder given it is the home folder, whatever else is said.
        assert_eq!(
            decode(&tokens(&["--moniker-title=T"]), home),
            Ok(Start {
                title: Some("T".into()),
                ..Start::home(home)
            })
        );
    }

    #[test]
    fn the_fields_may_come_in_any_order_and_the_arguments_keep_theirs() {
        let home = Path::new(HOME);
        let got = decode(
            &tokens(&[
                "--moniker-hold",
                "--moniker-arg=c",
                "--moniker-title=T",
                "--moniker-arg=a",
                "--moniker-dir=/tmp",
                "--moniker-arg=b",
                "--moniker-refused=/x",
            ]),
            home,
        )
        .expect("decodes");
        assert_eq!(
            got,
            Start {
                dir: PathBuf::from("/tmp"),
                title: Some("T".into()),
                run: Run::Command {
                    argv: vec!["c".into(), "a".into(), "b".into()],
                    hold: true
                },
                dir_refused: Some(PathBuf::from("/x")),
            }
        );
    }

    #[test]
    fn a_folder_that_is_not_utf8_starts_at_home_and_is_named() {
        let odd = PathBuf::from(OsString::from_vec(b"/tmp/b\xffd".to_vec()));
        let start = Start {
            dir: odd.clone(),
            ..a_start("/tmp", Run::Shell)
        };
        let written = encode(&start);
        assert_eq!(
            written,
            tokens(&["--moniker-refused=/tmp/b\u{fffd}d"]),
            "no folder is handed over, and the notification can name it"
        );
        let back = decode(&written, Path::new(HOME)).expect("decodes");
        assert_eq!(back.dir, Path::new(HOME));
        assert_eq!(back.dir_refused, Some(PathBuf::from("/tmp/b\u{fffd}d")));
        assert_eq!(back.run, Run::Shell);

        // A refused path that is not UTF-8 is shown the same way, and it
        // stays the one named when the folder is odd as well.
        let both = Start {
            dir: odd,
            dir_refused: Some(PathBuf::from(OsString::from_vec(b"/x/\xfe".to_vec()))),
            ..start
        };
        assert_eq!(encode(&both), tokens(&["--moniker-refused=/x/\u{fffd}"]));
        let just_refused = Start {
            dir_refused: both.dir_refused.clone(),
            ..a_start("/tmp", Run::Shell)
        };
        assert_eq!(
            encode(&just_refused),
            tokens(&["--moniker-dir=/tmp", "--moniker-refused=/x/\u{fffd}"])
        );
    }

    #[test]
    fn local_prints_the_help_the_version_and_mistakes() {
        let home = Path::new(HOME);
        let said = |list: &[&str]| local(&args(list), None, home);
        assert_eq!(
            said(&["--help"]),
            Local::Print {
                status: 0,
                to_stdout: true,
                text: HELP.to_string()
            }
        );
        assert_eq!(
            said(&["--version"]),
            Local::Print {
                status: 0,
                to_stdout: true,
                text: format!("moniker {}\n", env!("CARGO_PKG_VERSION"))
            }
        );
        assert_eq!(
            said(&["--frob"]),
            Local::Print {
                status: 2,
                to_stdout: false,
                text: "moniker: unknown option: --frob\n".to_string()
            }
        );
        assert_eq!(
            said(&["-e"]),
            Local::Print {
                status: 2,
                to_stdout: false,
                text: "moniker: -e needs a command to run\n".to_string()
            }
        );
        let bad = OsString::from_vec(vec![b'a', 0xff]);
        assert_eq!(
            local(&[OsString::from("--"), bad], None, home),
            Local::Print {
                status: 2,
                to_stdout: false,
                text: "moniker: arguments must be valid UTF-8\n".to_string()
            }
        );
        // The version is the package's, which is a version number.
        let Local::Print { text, .. } = said(&["--version"]) else {
            panic!("printed");
        };
        assert!(text.starts_with("moniker 0."), "{text:?}");
    }

    #[test]
    fn local_opens_a_window_by_handing_the_request_over() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("My Files").join("Ünï");
        std::fs::create_dir_all(&folder).unwrap();
        let text = folder.to_str().unwrap();
        let home = tmp.path().join("home");

        let handed = |list: &[&str], cwd: Option<&Path>| match local(&args(list), cwd, &home) {
            Local::HandOver(tokens) => tokens,
            other => panic!("{list:?} should open a window, got {other:?}"),
        };
        // What the desktop sends: a folder, then `--` and the command.
        let got = handed(
            &[
                &format!("--working-directory={text}"),
                "--title=Logs",
                "--hold",
                "--",
                "tail",
                "-f",
                "x y",
            ],
            None,
        );
        assert_eq!(
            got,
            vec![
                format!("--moniker-dir={text}"),
                "--moniker-title=Logs".to_string(),
                "--moniker-arg=tail".to_string(),
                "--moniker-arg=-f".to_string(),
                "--moniker-arg=x y".to_string(),
                "--moniker-hold".to_string(),
            ]
        );
        // The launching directory is resolved here, in the launching process.
        assert_eq!(
            handed(&[], Some(&folder)),
            vec![format!("--moniker-dir={text}")]
        );
        assert_eq!(
            handed(&["-w", "Ünï"], folder.parent()),
            vec![format!("--moniker-dir={text}")]
        );
        // `/` and an unknown directory give the home folder.
        let at_home = vec![format!("--moniker-dir={}", home.display())];
        assert_eq!(handed(&[], Some(Path::new("/"))), at_home);
        assert_eq!(handed(&[], None), at_home);
        // A folder that is not one is named, and the terminal starts at home.
        let missing = tmp.path().join("missing");
        assert_eq!(
            handed(&["-w", missing.to_str().unwrap()], None),
            vec![
                format!("--moniker-dir={}", home.display()),
                format!("--moniker-refused={}", missing.display()),
            ]
        );
    }

    #[test]
    fn what_local_hands_over_decodes_to_what_start_chooses() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("a b");
        std::fs::create_dir_all(&folder).unwrap();
        let home = tmp.path().join("home");
        let text = folder.to_str().unwrap();
        let file = tmp.path().join("file");
        std::fs::write(&file, "x").unwrap();
        for list in [
            vec![],
            vec!["--hold"],
            vec!["-w", text],
            vec!["-w", file.to_str().unwrap(), "-T", "t"],
            vec!["-T", "-- x", "-e", "sh", "-c", "echo $HOME; exit 3"],
            vec!["--", "--", "-e", "--help", ""],
            vec!["--hold", "-w", text, "--", "ls"],
        ] {
            let parsed = open(&list);
            let Local::HandOver(handed) = local(&args(&list), Some(&folder), &home) else {
                panic!("{list:?}");
            };
            assert_eq!(
                decode(&handed, &home),
                Ok(start(parsed, Some(&folder), &home)),
                "{list:?}"
            );
        }
    }

    #[test]
    fn a_person_cannot_type_the_handover() {
        // Only `local` makes these tokens: the same words typed as options are
        // mistakes, so a window can be asked for only through the options.
        let home = Path::new(HOME);
        for token in [
            "--moniker-dir=/tmp",
            "--moniker-arg=x",
            "--moniker-hold",
            "--moniker-title=t",
        ] {
            assert_eq!(
                local(&args(&[token]), None, home),
                Local::Print {
                    status: 2,
                    to_stdout: false,
                    text: format!("moniker: unknown option: {token}\n")
                },
                "{token}"
            );
        }
    }
}
