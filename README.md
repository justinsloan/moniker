# Moniker

Moniker is a GTK4 and libadwaita terminal emulator for Linux built around saved
SSH connections. It opens to a local terminal; a saved profile is one click away
in the new-tab list. A profile holds a host, user name, port, an optional key file
and a password; choosing it opens a logged-in tab, optionally running a fixed
startup sequence such as `apt update`. Sessions, local ones too, can be saved as
plain-text transcripts, which the Transcripts window lists, searches and shows.
The terminal itself is VTE, so rendering of htop, tmux and the like is VTE's.
This is alpha 4 (version 0.4.0).

Profiles and their passwords can be synced between your own machines, with no
server of anyone's: Moniker runs `moniker-server sync-serve` on the other
machine over plain `ssh` and exchanges one bundle encrypted with a shared sync
key. There are no split panes, no terminal search and no color settings (the
terminal colors follow the system light or dark style; Preferences has the font,
the scrollback and the transcripts). Moniker is released under the MIT license
(see `LICENSE`), copyright 2026 Justin M. Sloan.

Three programs are built: `moniker` (the terminal app), `moniker-server` (the
command line for sync and for headless machines, no GTK) and `moniker-askpass`
(the password helper ssh calls).

## Build and run

Needs a Rust toolchain (1.92 or newer for the GUI; the core and
`moniker-server` build with 1.89), a C compiler, pkg-config, GTK 4.18+, VTE 0.76+
and libadwaita 1.9+ with development files. On Ubuntu/Debian:

```
sudo apt install libvte-2.91-gtk4-dev libadwaita-1-dev
cargo build --workspace
target/debug/moniker
```

`cargo build --workspace` is required, not just `cargo run -p moniker-gui`: the
password helper `moniker-askpass` must sit next to `moniker` (and
`moniker-server`). Without it, stored passwords are not used and Moniker says
so.

`bash scripts/build-deb.sh` builds two Debian packages into `target/debian/`:
`moniker-server` (`moniker-server` and `moniker-askpass`, no GTK, for every
machine that syncs) and `moniker` (the terminal app, which depends on it). Each
carries the MIT license and the copyright line in its `copyright` file. Both
list Justin Sloan as Maintainer (the address comes from `git config user.email`),
and the `moniker` package carries AppStream metadata
(`data/org.moniker.Moniker.metainfo.xml`) that names the same developer, which
App Center's page for the installed app and GNOME Software should show as the
publisher (not yet confirmed on a desktop).

Passwords and the sync key live in the system keyring (Secret Service, e.g.
GNOME Keyring), which must be running and unlocked. The CLI and core need no GTK
or VTE, so a headless machine can build just `cargo build --release -p moniker-cli`.

## Using Moniker

* **Launch** opens a window with one local terminal (your login shell, in your
  home directory). With one tab there is no tab bar.
* **Header bar**: **＋** opens a new tab, and **☰** holds a text size row, New
  Window, Connection Profiles, Transcripts, Sync Now (once sync is set up),
  Preferences, Keyboard Shortcuts and About. The window title is the tab's title.
  There is no Connect button: you connect from the new-tab list or the
  Connection Profiles window.
* **New tab** (＋ or Ctrl+Shift+T) shows a searchable list: **Local terminal**
  first, then every profile. Press Enter on Local terminal for a shell at once;
  pick a profile and that same tab becomes its session. The **›** on Local
  terminal shows its choices: Open, and a "Save transcript" box.
* **Connect** to a profile by clicking it in the new-tab list or pressing Enter:
  that logs in and gives you the normal shell, saving a transcript if the profile
  is set to (or if "Save transcripts of all sessions" is on, see Preferences).
  The **›** on a row (or Right arrow) shows the choices for that profile instead:
  "Just connect", one entry per startup sequence, and a "Save transcript" box
  that starts as the profile's setting (checked when every session is saved) and
  overrides it for that connection, in either direction.
* **Tabs**: Ctrl+Shift+T new, Ctrl+Shift+W close, Ctrl+PgUp / Ctrl+PgDn switch,
  Ctrl+Shift+N new window. Every other key, Ctrl+Tab and F10 included, goes to
  the program in the terminal (Ctrl+C is still interrupt), apart from the copy,
  paste and text size keys below and Ctrl+comma for Preferences. Double-click a
  tab (or right-click it, then Rename…) to rename it. A remote tab is named after
  its profile and the remote shell cannot change that; a local tab follows its
  shell's title until you rename it. Drag a tab out of the bar to make a window
  of it, or into another Moniker window. Closing the last tab closes the window,
  and closing the last window quits (a move of transcripts in progress, see
  Preferences, finishes first). Closing a tab or a window hangs up whatever runs
  in it: the `ssh`, or the shell and its job. While a dialog such as Rename,
  About or Preferences is open, the tab, copy, paste and text size keys leave the
  tab behind it alone (Ctrl+Shift+V pastes into the dialog's field).
* **Copy and paste**: select text with the mouse, then Ctrl+Shift+C copies it
  and Ctrl+Shift+V pastes the clipboard (Ctrl+C stays interrupt, and
  Ctrl+Shift+C with nothing selected does nothing). Right-click in a terminal
  for a menu with Copy and Paste; when the program in the terminal is using the
  mouse (htop, mc, tmux with mouse on), a plain right-click goes to the program
  and Shift+right-click opens the menu. A multi-line paste reaches a shell that
  supports bracketed paste as one block instead of running line by line.
  Middle-click still pastes the selection.
* **Text size**: Ctrl+plus (also Ctrl+= and keypad +) makes the text larger,
  Ctrl+minus (keypad −) smaller and Ctrl+0 returns to normal, from 50% to 300%.
  The ☰ menu has the same as − 100% +. Each tab has its own size, and a new tab
  starts at the size of the tab you were in. When you close the last window, the
  size of its active tab is remembered for the next launch's first tab. (Ctrl+minus
  no longer reaches programs in the terminal; readline's undo is still on
  Ctrl+_.)
* **Connection Profiles** (☰ → Connection Profiles) opens a separate window laid
  out like GNOME Settings: a sidebar of profiles (＋ adds one, the magnifier
  searches) and the selected profile's page. Double-click or Enter on a profile,
  or the page's Connect button, opens a session in the main window you used last.
  Narrow the window and it shows one pane at a time with a back button.
* **Editing a profile** saves field by field: a text field shows a ✓ when you
  change it, and the value is saved only when you press ✓ or Enter. Leave the
  field without that and your edit is dropped. Switches, the port and the key
  file save when changed. A mistake (an empty host, say) marks the field and
  a notification says why; nothing is saved and the field keeps what you typed.
  The password field is empty unless you type a new one, and an empty field keeps
  the stored password; "Remove stored password" deletes it from this machine only.
* **Startup sequences** are listed on the profile page, one row each. Open one to
  name it, type its commands (one per line) and Save or Cancel; Move Up and Move
  Down set the order they appear in the Connect choices. The commands run on the
  remote after login, then you get your normal shell. A profile with no sequences
  gives a plain shell.
* **Transcripts** are the plain text of a session, written to
  `~/.local/share/moniker/transcripts/<host>_<date>-<time>.txt` (UTC, mode 0600)
  when the tab ends or closes, once, wherever the tab is by then. A local
  terminal's transcript is named after this computer. Whether a session is saved
  is decided before its tab opens: a profile has a "Save transcript" switch, the
  **›** choices of a profile and of Local terminal have a "Save transcript" box,
  and Preferences has "Save transcripts of all sessions" (off to start with),
  which saves every new tab, the first tab at launch included, whatever the
  profile says; the box under **›** then starts checked, and unchecking it skips
  that one session. Transcripts are never synced. The folder is chosen in
  Preferences.
* **Transcripts window** (☰ → Transcripts) opens a window, laid out like the
  Connection Profiles window, that looks back at your transcripts. The sidebar
  lists every transcript under day headings (Today, Yesterday, then dates) with
  its host and local start time; choose Newest first or Oldest first, narrow it
  to one host, or type in the search entry to keep only the transcripts that
  contain the text, each shown with its match count. Only files named the way
  Moniker names transcripts (`<host>_<date>-<time>.txt`) are listed; anything
  else in the folder is ignored (never read, searched or offered for deletion),
  so a folder you also use for other things is safe to choose. Choosing a
  transcript shows its whole text, read-only. Its own search bar highlights
  every match: Enter or ↓ goes to the next, ↑ to the previous, and Ctrl+F
  focuses it. **Export…** saves a copy wherever you choose (mode 0600, like the
  original; exporting onto the transcript itself is refused), and **Delete**
  asks, then removes the file. A transcript finished while the window is open
  appears in the list, and the window follows the folder if you change it in
  Preferences. Transcripts hold whatever was on screen, which may include
  secrets, and the window says so. A line of more than 2,000 characters is
  shown broken into lines of 2,000, so that a transcript of one enormous line
  opens quickly; the file and Export keep the original text and search finds a
  match across a break. Copying a selection (Ctrl+C or Copy in the right-click
  menu) gives the original text, without those added line breaks; a
  middle-click paste or a drag of the selection still carries them, and Export
  gives the exact file.
* **Preferences** (☰ → Preferences, or Ctrl+comma) opens a dialog.
  **Terminal**: the **font** (a chooser that offers monospace fonts only; it
  applies to every open terminal at once) and the **scrollback**, 1,000 to
  1,000,000 lines (10,000 to start with), for tabs opened after the change; a tab
  that saves a transcript keeps all its scrollback. **Transcripts**: "Save
  transcripts of all sessions" and the **transcripts folder** with a Choose…
  button. Choosing another folder **moves the transcripts in the old one into
  it**, in the background: files are renamed into place, or copied and then
  removed when the folders are on different file systems; a name already taken
  gets a number (`-2`, `-3`) and nothing is overwritten; and a message says how
  many were moved and, if anything stayed behind, names the old folder. Only
  files named like Moniker's transcripts move. Any other file in the old folder
  stays, and the message counts them, so choosing your home folder once cannot
  take your documents with it. A session that is saving a transcript meanwhile
  saves it into the new folder. **Sync**: Open shows the Sync page of the
  Connection Profiles window. Each change is saved at once and is local to this
  machine, and a message about one that could not be saved appears in the
  dialog.
* Startup commands are stored as plain text, and transcripts record whatever
  appears in the terminal, including what you type or paste. Keep passwords out
  of both. The profile page and Preferences say so.
* **Messages** appear as notifications at the top center of the window, just
  below the header bar: they wrap onto several lines, fade after a few seconds
  (a little longer for a long message) and come one after another if there are
  several. For that time one covers whatever is under it, such as the top of the
  terminal. They say, for instance, that a transcript could not be saved or that
  `moniker-askpass` is missing, give the result of a sync, and in the Transcripts
  window the result of an export or a failed delete. While a dialog is open the
  window's messages wait and appear when it closes; Preferences shows its own
  in the dialog, at the bottom. A message with no window left to show it (a
  transcripts move that ends after the last window closed) is written to the
  standard error of the `moniker` process instead.

Profile data (no secrets) is in `~/.config/moniker/profiles.json`. Which profile
is your sync partner, whether to sync automatically, the text size the next
launch starts at and the Preferences (font, scrollback, saving all transcripts,
the transcripts folder) are in `~/.config/moniker/gui-settings.json` (also local
to this machine, mode 0600). If that file is damaged, Moniker uses the defaults
and, when it quits, leaves the file as it is instead of overwriting it; a
Preferences change then says it cannot be saved and leaves it too.

## Syncing two machines

1. **Install Moniker on both.** The other machine needs only the CLI:
   `cargo build --release -p moniker-cli`, then
   `sudo install -m 755 target/release/moniker-server target/release/moniker-askpass /usr/local/bin/`.
   `moniker-server` must be on the remote's *non-interactive* `PATH`;
   `~/.cargo/bin` usually is not. Put it in `/usr/local/bin` or `~/.local/bin`
   and check that `ssh remote-host which moniker-server` prints a path.
2. **Set the same sync key on every machine**, at least 12 characters, with
   `moniker-server sync-key set` (asks twice, no echo; `--stdin` reads it from a
   pipe) or on the Sync page of the Connection Profiles window. It is stored in
   the keyring and never sent.
3. **The other machine needs an unlocked Secret Service** that a non-interactive
   ssh session can reach, because received passwords are stored there. Without
   one the sync fails with a message and changes nothing.
4. **Save a profile for the other machine** and **connect to it once from a tab**
   to accept its host key. Sync never asks about host keys; it refuses unknown ones.
5. **Sync**: the Sync button on the Sync page of the Connection Profiles window
   (choose the other machine's profile there), or
   `moniker-server sync <profile name>` (names must be unique). Profiles added on
   either side are kept, the newest edit of a profile wins, and deletions
   propagate.
6. **Automatic sync**: once a sync from the Sync page has succeeded, that profile
   is your sync partner. From then on Moniker syncs with it each time it launches
   (in the background; the terminal never waits, and a success is silent), and
   ☰ → Sync Now does it on request. A failure is one notification. The Sync page
   has a switch to turn the launch sync off. It never asks anything in a Moniker
   tab and never accepts a host key; see the known limits for a prompt that can
   still appear in the shell that started `moniker`.

## Security model

* Passwords and the sync key are kept in the keyring, never in a file, in `argv`
  or in the environment. `ssh` is pointed at `moniker-askpass`, which reads the
  keyring itself and answers only a password prompt that names exactly that
  profile's user and host, and only once per connection (a one-time marker file
  in your runtime directory); host-key and key-passphrase prompts, and any
  second password prompt, go to you.
* The sync bundle is encrypted (Argon2id, XChaCha20-Poly1305) with the sync key
  before it leaves the machine; ssh is only the transport. The client sends its
  bundle before the other end has shown it knows the key, so use a long key.
* A machine holding your sync key is trusted: it sees every password in the
  bundle and can add, change or delete your profiles. Received profiles are
  validated (host and user names are restricted, control characters refused).
* Config, transcript and exported transcript files are mode 0600 (a transcripts
  folder Moniker creates is 0700). Moniker makes no network connections other
  than your ssh sessions.

## Known limits

* The stored password is used only when ssh's prompt names exactly the profile's
  user and host. A `HostName` alias in `~/.ssh/config` or a ProxyJump hop makes
  ssh show another name, and then you type the password.
* A wrong stored password is tried once; then ssh asks for the password in the
  terminal, as plain ssh does (three attempts in all). The stored password is
  never sent twice in one connection. A sync has no terminal: a wrong stored
  password fails the sync.
* Transcripts hold what VTE holds: content shown only on the alternate screen
  (htop, vim) is not in them, and a crash loses the transcript in progress.
* Removing a stored password on the profile page is local to that machine; others
  keep theirs, and a later sync from one that still has it can bring it back.
* The text size is saved when the last window is closed. If Moniker is killed, or
  the session ends without closing its windows, the previously saved size stays.
* Closing a tab hangs up its process. One that ignores the hang-up keeps running,
  and its terminal is kept until it exits.
* The Transcripts window reads a transcript whole to show or search it (off the
  UI thread), so a very large transcript, or a very large folder searched at
  once, takes time and memory. It lists only files named like transcripts, so a
  transcript you renamed no longer appears in it (the file stays where it is).
  If an export onto an existing file fails part-way, that file is left partly
  written.
* Changing the transcripts folder moves only files named like transcripts. When
  the folders are on different file systems each file is copied, then removed;
  Moniker waits for the move to end before it quits, but a kill or a logout in
  the middle can leave a truncated copy in the new folder (the original is still
  whole in the old one).
* Preferences cannot go back to the system monospace font once a font is chosen;
  removing `font` from `gui-settings.json` does it.
* Some keyring calls run on the UI thread (opening the Sync page, saving a sync
  key, connecting, saving a password on the profile page, removing a stored
  password, deleting a profile, and creating a profile with a password); with a
  locked keyring the window can freeze briefly. A sync reads the keyring on its
  own thread.
* A sync's `ssh` has no `BatchMode`, so when you start `moniker` from a shell, a
  key-passphrase, ProxyJump or password prompt for a sync (the automatic one at
  launch too) can appear in that shell's terminal.
* Sync needs a Secret Service on the remote machine; there is no fallback store.

## License

Moniker is released under the MIT license; see `LICENSE`. Copyright 2026 Justin
M. Sloan.

## Tests

```
cargo test --workspace
bash scripts/check-no-gtk.sh      # core and CLI must not depend on GTK, VTE or libadwaita
bash scripts/gui-tests.sh         # the window tests, under a virtual display
bash scripts/e2e-sync.sh          # two instances over real sshd, private bus/keyring
```

The window tests open real GTK windows and run real terminals with a fake `ssh`,
so `cargo test` skips them (they are `#[ignore]`d) and they need `xvfb-run`
(`apt install xvfb`). Run them only with `scripts/gui-tests.sh [name filter]`: it
builds the test binary and runs it with an empty environment, a throwaway `HOME`
and `XDG_*` directories, a private virtual display and a session bus address that
points nowhere, so it cannot reach your desktop, your keyring or your files. It
starts no bus or keyring and kills nothing.

`scripts/e2e-sync.sh` is the only thing that starts a bus, a keyring and an
`sshd`, all private, and it fails if your real session files change. Its
"`moniker-server` not on remote PATH" check fails on a machine that has
`moniker-server` installed in `/usr/bin`.

`docs/manual-verification.md` lists what needs a real desktop and a real host.
