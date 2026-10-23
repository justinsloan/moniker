<h1 align="center">Moniker</h1>

<p align="center">
  <strong>A Linux terminal built around saved SSH connections.</strong><br>
  One click to a logged-in shell, startup commands that run by themselves, and credential sync between your own machines with no server in the middle.
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="MIT License"></a>
  <a href="https://github.com/justinsloan/moniker/releases/latest"><img src="https://img.shields.io/github/v/release/justinsloan/moniker?label=release" alt="Latest release"></a>
  <a href="https://github.com/justinsloan/moniker/actions/workflows/ci.yml"><img src="https://github.com/justinsloan/moniker/actions/workflows/ci.yml/badge.svg?branch=main" alt="CI status"></a>
  <img src="https://img.shields.io/badge/rust-1.92%2B-orange.svg?logo=rust&logoColor=white" alt="Rust 1.92+">
  <img src="https://img.shields.io/badge/GTK%204-libadwaita-4A86CF.svg?logo=gnome&logoColor=white" alt="GTK 4 and libadwaita">
  <img src="https://img.shields.io/badge/platform-linux-lightgrey.svg?logo=linux&logoColor=white" alt="Platform: Linux">
</p>

<p align="center">
  <img src="docs/screenshots/startup-sequence.png" alt="Moniker with three tabs; the web-01 tab has logged in, run its Update startup sequence (apt update) and left a shell prompt" width="900">
</p>

---

## Why Moniker?

If you log in to the same machines many times a day, an ordinary terminal has you:

- **Typing the same login again and again.** Host, user name and password, every time.
- **Repeating the same commands after every login**, such as `apt update`.
- **Keeping hosts and passwords in step by hand** across a laptop and a desktop, or handing them to a vendor's cloud to sync them for you, usually behind a subscription.

Moniker is a terminal first. VTE, the engine behind GNOME Terminal and Console, renders htop, tmux and vim, and Moniker adds no per-frame work on top of it. Next to that, a saved **connection profile** is one click from a logged-in shell, and profiles sync between your own machines over plain SSH, encrypted with a key that only those machines hold.

> [!NOTE]
> Moniker is alpha software (0.4.0, alpha 4). Expect rough edges, and see the [known limitations](#known-limitations).

## Features

- 🔑 **One click to a logged-in shell.** A profile holds the host, user name, port, an optional key file and a password. Pick it in the new-tab list and the tab logs in with no typing. The password lives in the system keyring and reaches `ssh` through a helper that answers only that host's password prompt, never through a file, the command line or the environment.
- ▶️ **Startup sequences.** Give a profile as many named command lists as you like ("update", "tail the logs"). Connect with one and its commands run after login, then you are left in your normal interactive shell.
- 🔄 **Sync with no server.** Profiles and passwords sync between your own machines over plain SSH. The bundle is encrypted with a shared sync key (Argon2id, XChaCha20-Poly1305) before it leaves the machine, and the key itself is never sent. Changes merge both ways: profiles added on either side are kept, the newest edit of a profile wins and deletions propagate. Once set up, Moniker syncs in the background at every launch.
- 📝 **Transcripts.** Save a plain-text transcript of any session, remote or local: per profile, per connection, or of every session. The Transcripts window lists them by day, filters by host, searches across all of them, and exports or deletes them. Transcripts never leave the machine.
- 🖥️ **A real terminal.** Tabs, several windows, tabs dragged from one window to another, a text size per tab, and copy and paste with bracketed paste. Moniker keeps only a handful of [shortcuts](#keyboard-shortcuts); every other key, Ctrl+Tab and F10 included, goes to the program in the terminal.
- 🎨 **The modern GNOME look.** libadwaita, following the system light or dark style, terminal colors close to GNOME Console's, and a Connection Profiles window laid out like GNOME Settings.
- 🗄️ **Headless machines too.** `moniker-server` is a command line with no GTK, for a server that only takes part in sync.
- 🚫 **No cloud, no account, no telemetry.** The only network traffic is your own `ssh` connections.

## Screenshots

<table>
  <tr>
    <th>New-tab list</th>
    <th>Connect choices</th>
  </tr>
  <tr>
    <td><img src="docs/screenshots/new-tab.png" alt="The new-tab list: Local terminal, then the saved profiles with their user and host" width="440"></td>
    <td><img src="docs/screenshots/connect-choices.png" alt="The choices behind a profile's arrow: Just connect, each startup sequence by name, and a Save transcript box" width="440"></td>
  </tr>
  <tr>
    <th colspan="2">htop over SSH, started by a startup sequence</th>
  </tr>
  <tr>
    <td colspan="2" align="center"><img src="docs/screenshots/htop.png" alt="The db-01 tab running htop on a PostgreSQL server" width="890"></td>
  </tr>
</table>

## Getting started

### Download

`.deb` and `.rpm` packages for x86-64 are on the [Releases page](https://github.com/justinsloan/moniker/releases/latest). They need **Ubuntu 26.04** or **Fedora 44** or newer, the first releases with libadwaita 1.9. Each release has two packages: `moniker`, the terminal app, and `moniker-server`, the command line and the password helper, which `moniker` depends on.

```sh
# Ubuntu
sudo apt install ./moniker_0.4.0_amd64.deb ./moniker-server_0.4.0_amd64.deb
# Fedora
sudo dnf install ./moniker-0.4.0-1.fc44.x86_64.rpm ./moniker-server-0.4.0-1.fc44.x86_64.rpm
```

A headless machine that only syncs needs just `moniker-server`. Passwords and the sync key are kept in the system keyring (a Secret Service such as GNOME Keyring), which must be running and unlocked.

To check a download against the release's `SHA256SUMS`, and, with GitHub CLI 2.49 or newer (Ubuntu 26.04's own `gh` package is 2.46), that GitHub Actions built it from this repository:

```sh
sha256sum --ignore-missing -c SHA256SUMS
gh attestation verify moniker_0.4.0_amd64.deb --repo justinsloan/moniker
```

### Run from source

Requires Rust 1.92 or newer (the core and `moniker-server` build with 1.89), a C compiler, pkg-config, and the development files of GTK 4.18+, VTE 0.76+ and libadwaita 1.9+.

```sh
sudo apt install build-essential pkg-config libadwaita-1-dev libvte-2.91-gtk4-dev   # Ubuntu
sudo dnf install gcc pkgconf-pkg-config libadwaita-devel vte291-gtk4-devel           # Fedora
git clone https://github.com/justinsloan/moniker.git
cd moniker
cargo build --workspace
target/debug/moniker
```

Build with `--workspace`, not just `cargo run -p moniker-gui`: the password helper `moniker-askpass` must sit next to `moniker`. Without it Moniker cannot use stored passwords, and it says so. On a headless machine, `cargo build --release -p moniker-cli` builds only `moniker-server` and `moniker-askpass`, with no GTK.

## Using Moniker

Moniker opens with one local terminal: your login shell, in your home directory. The header bar has **＋** for a new tab and **☰** for everything else.

| Action | How |
|---|---|
| **New tab** | **＋** or <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>T</kbd> shows a searchable list: **Local terminal**, then every profile. What you pick turns that same tab into the session. |
| **Connect** | Click a profile, or press <kbd>Enter</kbd> on it. It logs in and leaves you in the normal shell. |
| **Connect with options** | **›** on a row (or <kbd>→</kbd>): **Just connect**, each startup sequence by name, and a **Save transcript** box for this one connection. Local terminal has a **›** too. |
| **Add or edit profiles** | **☰ → Connection Profiles**. Double-click a profile there, or press its **Connect** button, to open a session in the main window you used last. |
| **Read transcripts** | **☰ → Transcripts**. |
| **Sync** | **☰ → Sync Now**, once sync is set up (see [Syncing two machines](#syncing-two-machines)). |
| **Rename a tab** | Double-click it, or right-click it, then **Rename…**. |
| **New window** | **☰ → New Window**, or drag a tab out of the tab bar. A tab can also be dragged into another Moniker window. |
| **Text size** | The row at the top of **☰** (− 100% +), or the keys below. |

### Keyboard shortcuts

| Keys | Action |
|---|---|
| <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>T</kbd> / <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>W</kbd> | New tab / close tab |
| <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>N</kbd> | New window |
| <kbd>Ctrl</kbd>+<kbd>PgUp</kbd> / <kbd>Ctrl</kbd>+<kbd>PgDn</kbd> | Previous / next tab |
| <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>C</kbd> / <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>V</kbd> | Copy / paste |
| <kbd>Ctrl</kbd>+<kbd>+</kbd> / <kbd>Ctrl</kbd>+<kbd>−</kbd> / <kbd>Ctrl</kbd>+<kbd>0</kbd> | Larger / smaller / normal text, in this tab |
| <kbd>Ctrl</kbd>+<kbd>,</kbd> | Preferences |

Every other key, <kbd>Ctrl</kbd>+<kbd>C</kbd>, <kbd>Ctrl</kbd>+<kbd>Tab</kbd> and <kbd>F10</kbd> included, goes to the program in the terminal. Larger text also answers to <kbd>Ctrl</kbd>+<kbd>=</kbd>, and all three text size keys work on the keypad. One trade-off: <kbd>Ctrl</kbd>+<kbd>−</kbd> no longer reaches the program; readline's undo is still on <kbd>Ctrl</kbd>+<kbd>_</kbd>. **☰ → Keyboard Shortcuts** shows the same list.

### Tabs and windows

- With one tab there is no tab bar. A remote tab is named after its profile, and the remote shell cannot change that; a local tab follows its shell's title until you rename it.
- Closing a tab or a window hangs up whatever runs in it: the `ssh`, or the shell and its job. Closing the last window quits.
- Each tab has its own text size, from 50% to 300%. A new tab starts at the size of the tab you were in, and the size of the last window's active tab is remembered for the next launch.
- While a dialog such as Rename, About or Preferences is open, the tab, copy, paste and text size keys leave the tab behind it alone, so <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>V</kbd> pastes into the dialog's field.
- **Copy and paste:** select with the mouse and press <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>C</kbd>; <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>V</kbd> pastes. Right-click a terminal for a menu with Copy and Paste. When the program is using the mouse (htop, mc, tmux with mouse on), a plain right-click goes to the program and <kbd>Shift</kbd>+right-click opens the menu. A multi-line paste reaches a shell that supports bracketed paste as one block instead of running line by line. Middle-click still pastes the selection.

### Profiles and startup sequences

- **Editing saves field by field.** A changed text field shows a ✓, and the value is saved only when you press ✓ or <kbd>Enter</kbd>; leave the field without that and the edit is dropped. Switches, the port and the key file save when changed. A mistake, such as an empty host, marks the field and a notification says why, and nothing is saved.
- **Passwords:** the password field is empty unless you type a new one, and leaving it empty keeps the stored password. **Remove stored password** deletes it from this machine only.
- **Startup sequences** are listed on the profile page, one row each. Open one to name it and type its commands, one per line, then Save or Cancel; Move Up and Move Down set the order of the Connect choices. A profile with no sequences gives a plain shell.
- Narrow the Connection Profiles window and it shows one pane at a time, with a back button.

### Transcripts

A transcript is the plain text of a session, written when its tab ends or closes, to `~/.local/share/moniker/transcripts/<host>_<date>-<time>.txt` (UTC, mode 0600) unless Preferences chose another folder. A local terminal's transcript is named after this computer. Whether a session is saved is decided before its tab opens:

| Setting | Where | Effect |
|---|---|---|
| **Save transcript** switch | A profile's page | Every connection to that profile is saved. |
| **Save transcript** box | The **›** choices of a profile or of Local terminal | Overrides the setting for this one session, in either direction. |
| **Save transcripts of all sessions** | Preferences (off to start with) | Every new tab is saved, the first one at launch included, whatever the profile says. The box under **›** then starts checked. |

The **Transcripts window** lists them under day headings (Today, Yesterday, then dates) with the host and the local start time. Sort Newest first or Oldest first, narrow the list to one host, or type in the search entry to keep only the transcripts that contain the text, each with its match count. Choosing one shows its whole text, read-only, with a search bar of its own that highlights every match: <kbd>Enter</kbd> or <kbd>↓</kbd> for the next, <kbd>↑</kbd> for the previous, <kbd>Ctrl</kbd>+<kbd>F</kbd> to focus it. **Export…** saves a copy (mode 0600, like the original) and **Delete** asks, then removes the file. The list follows the folder live, so a session that ends while the window is open appears in it.

Only files named the way Moniker names transcripts are listed; anything else in the folder is never read, searched or offered for deletion, so a folder you also use for other things is safe to choose. A line longer than 2,000 characters is shown broken into lines of 2,000, so that a transcript of one enormous line opens quickly. The file keeps the original text, search finds a match across a break, and copying a selection gives the original text, without the added breaks.

> [!WARNING]
> Transcripts record whatever appears in the terminal, and startup sequences are stored as plain text. Both can hold secrets you type or paste. Keep passwords out of them. Transcripts are never synced.

### Preferences

**☰ → Preferences** or <kbd>Ctrl</kbd>+<kbd>,</kbd>. Each change is saved at once and stays local to this machine.

| Group | Setting | Notes |
|---|---|---|
| Terminal | **Font** | Monospace fonts only. Applies to every open terminal at once. |
| Terminal | **Scrollback** | 1,000 to 1,000,000 lines (10,000 to start with), for tabs opened after the change. A tab that saves a transcript keeps all of its scrollback. |
| Transcripts | **Save transcripts of all sessions** | See [Transcripts](#transcripts). |
| Transcripts | **Transcripts folder** | **Choose…** picks another folder and moves the transcripts into it (below). |
| Sync | **Sync settings** | **Open** shows the Sync page of the Connection Profiles window. |

**Changing the transcripts folder** moves the old folder's transcripts into the new one, in the background. Files are renamed into place, or copied and then removed when the folders are on different file systems. A name already taken gets a number (`-2`, `-3`) and nothing is ever overwritten. Only files named like Moniker's transcripts move: any other file stays where it is and the message counts it, so choosing your home folder once cannot take your documents with it. A session saving a transcript meanwhile saves it into the new folder. When it is done, a message says how many were moved and, if anything stayed behind, names the old folder.

### Messages

What needs your attention (a transcript that could not be saved, `moniker-askpass` missing, the result of a sync, an export or a failed delete) appears as a notification at the top center of the window, just below the header bar. Notifications wrap onto several lines, fade after about 8 seconds (a little longer for a long one) and come one after another. They cannot be dismissed, so for that time one covers whatever is under it. While a dialog is open the window's notifications wait until it closes; Preferences shows its own at the bottom of the dialog. A message with no window left to show it goes to the standard error of the `moniker` process.

### Where Moniker keeps things

| What | Where | Synced |
|---|---|---|
| Passwords and the sync key | The system keyring (Secret Service) | Passwords yes, the sync key never |
| Profiles, no secrets | `~/.config/moniker/profiles.json` (mode 0600) | Yes |
| Sync partner, automatic sync, text size for the next launch, Preferences | `~/.config/moniker/gui-settings.json` (mode 0600) | No |
| Transcripts | `~/.local/share/moniker/transcripts/`, or the folder chosen in Preferences | No |

If `gui-settings.json` is damaged, Moniker uses the defaults and leaves the file as it is instead of overwriting it; a Preferences change then says it cannot be saved.

## Syncing two machines

1. **Install Moniker on both.** The other machine needs only `moniker-server`: install its package, or run `cargo build --release -p moniker-cli` and then `sudo install -m 755 target/release/moniker-server target/release/moniker-askpass /usr/local/bin/`.
2. **Set the same sync key on every machine**, at least 12 characters, with `moniker-server sync-key set` (it asks twice and does not echo; `--stdin` reads it from a pipe) or on the Sync page of the Connection Profiles window. It is stored in the keyring and never sent.
3. **Make sure the other machine has an unlocked Secret Service** that a non-interactive ssh session can reach: received passwords are stored there. Without one the sync fails with a message and changes nothing.
4. **Save a profile for the other machine and connect to it once from a tab** to accept its host key. Sync never asks about host keys; it refuses unknown ones.
5. **Sync** from the Sync page of the Connection Profiles window, after choosing the other machine's profile there, or with `moniker-server sync <profile name>` (the name must be unique).
6. **From then on it is automatic.** Once a sync from the Sync page has succeeded, that profile is your sync partner: Moniker syncs with it at each launch, in the background (the terminal never waits, and a success is silent), and **☰ → Sync Now** syncs on request. A failure is one notification. The Sync page has a switch to turn the launch sync off.

> [!IMPORTANT]
> `moniker-server` must be on the other machine's *non-interactive* `PATH`, and `~/.cargo/bin` usually is not. Install it in `/usr/local/bin` or `~/.local/bin` (the packages put it in `/usr/bin`), then check that `ssh other-machine which moniker-server` prints a path.

## How it keeps your passwords safe

Connecting with a stored password passes through these layers:

```text
 you pick a profile
     │
     ▼
 ① The system's own ssh ──── Started with an argument list, never a shell string, and no
     │                        password in it. ~/.ssh/config, known_hosts and ssh-agent apply.
     ▼
 ② SSH_ASKPASS ───────────── ssh asks moniker-askpass. The environment carries only the
     │                        profile id and a one-time token, never a secret.
     ▼
 ③ Exact prompt match ────── The helper answers only the password prompt for exactly this
     │                        profile's user@host. Host-key questions, key passphrases and
     │                        ProxyJump hops go to you.
     ▼
 ④ One-time marker ───────── At most one answer per connection: after a wrong stored
     │                        password, ssh asks you, and the stored one is never sent again.
     ▼
 ⑤ System keyring ────────── The password is read from the Secret Service at that moment.
                              It is never written to a file.
```

A sync exchanges sealed bundles over ssh. The sync key never goes on the wire:

```text
 this machine                                           the other machine
 moniker ─────── ssh -T, StrictHostKeyChecking=yes ───▶ moniker-server sync-serve
    │                                                      │
    │  HELLO ───────────────────────────────────────────▶  │  sync key set? Secret Service?
    │  ◀─────────────────────────────────────────── HELLO  │
    │  sealed profiles and passwords ───────────────────▶  │  open, validate every entry,
    │                                                      │  merge, apply
    │  ◀───────────────────────────── sealed merged set    │
    ▼                                                      ▼
 open, validate, apply                                  done
```

*Sealed* means encrypted and authenticated with XChaCha20-Poly1305, under a key that Argon2id derives from the sync key. Each side changes its own state only after reading the whole bundle from the other, and stores passwords before the profile file, so a failure part-way never leaves a profile without its password, and the next sync catches up.

- **A machine holding your sync key is trusted.** It sees every password in the bundle and can add, change or delete your profiles. What it sends is still checked at the edge: ids must be canonical UUIDs, host and user names are restricted, control characters are refused, and one bad entry rejects the whole sync.
- **Use a long sync key.** The client sends its bundle once the other end has answered, but before it has shown that it knows the key.
- **Names stay plain text.** Profile names, hosts and tab titles are never shown as markup, and a remote shell cannot retitle its tab to look like another host.
- **Files are private.** Config, transcript and exported transcript files are mode 0600, and a transcripts folder Moniker creates is 0700.
- **No other traffic.** Moniker makes no network connections other than your ssh sessions.

## Building packages

```sh
bash scripts/build-deb.sh   # → target/debian/
bash scripts/build-rpm.sh   # → target/rpm/ (on Fedora 44 or newer, needs rpm-build)
```

| File | Contents |
|---|---|
| `target/debian/moniker_<version>_amd64.deb` | The terminal app; depends on `moniker-server` |
| `target/debian/moniker-server_<version>_amd64.deb` | `moniker-server` and `moniker-askpass`, no GTK |
| `target/rpm/moniker-<version>-1.fc<NN>.x86_64.rpm` | The terminal app |
| `target/rpm/moniker-server-<version>-1.fc<NN>.x86_64.rpm` | `moniker-server` and `moniker-askpass`, no GTK |

Build each package on the distribution it is for, so that it links that distribution's libraries. Both scripts take the maintainer's address from `git config user.email` and refuse to build without it. The packages name Justin Sloan as maintainer, carry the MIT license and give the repository as their homepage, and the `moniker` package includes AppStream metadata (`data/org.moniker.Moniker.metainfo.xml`), from which App Center and GNOME Software take the publisher's name.

## Development

```text
crates/
├── moniker-core/    GTK-free library: profiles, storage, keyring, crypto, merge, sync,
│                    ssh command lines, the askpass logic, transcripts
├── moniker-cli/     moniker-server (profiles, sync key, sync) and moniker-askpass
└── moniker-gui/     moniker: the GTK 4 / libadwaita / VTE app and its window tests
data/                Desktop file and AppStream metadata
scripts/             Package builds, test runners, the CI install test
docs/                Screenshots, and hands-on checks for a real desktop and host
```

The core and the CLI must not depend on GTK or VTE, because a sync partner may be a headless server; `scripts/check-no-gtk.sh` enforces it.

### Tests

```sh
cargo test --workspace
bash scripts/check-no-gtk.sh   # core and CLI must not depend on GTK, VTE or libadwaita
bash scripts/gui-tests.sh      # the window tests, under a virtual display
bash scripts/e2e-sync.sh       # two instances over a real sshd, with a private bus and keyring
```

The window tests open real GTK windows and run real terminals with a fake `ssh`. `cargo test` skips them (they are `#[ignore]`d), and they need `xvfb-run` (`apt install xvfb`). `docs/manual-verification.md` lists what needs a real desktop and a real host.

> [!IMPORTANT]
> Run the window tests only through `scripts/gui-tests.sh [name filter]`. It runs the test binary with an empty environment, a throwaway `HOME` and `XDG_*` directories, a private virtual display and a session bus address that points nowhere, so the tests cannot reach your desktop, your keyring or your files; it starts no bus or keyring and kills nothing. `scripts/e2e-sync.sh` is the only script that starts a bus, a keyring and an `sshd`, all private, and it fails if your real session files change. Its "`moniker-server` not on remote PATH" check fails on a machine that has `moniker-server` installed in `/usr/bin`.

### Continuous integration and releases

GitHub Actions (`.github/workflows/ci.yml`) runs on every push and pull request. In an Ubuntu 26.04 container it checks formatting, runs clippy, `cargo test`, `check-no-gtk.sh` and the window tests, and validates the desktop file and the AppStream metadata. It then builds the `.deb` on Ubuntu 26.04 and the `.rpm` on Fedora 44, and installs and starts each in a fresh container (`scripts/install-test.sh`, which installs packages as root and so runs only in a throwaway container). `e2e-sync.sh` is not run there yet.

To make a release, raise `version` in `Cargo.toml`, add the version to the release list in `data/org.moniker.Moniker.metainfo.xml`, commit, and push a tag named after it:

```sh
git tag v0.5.0
git push origin v0.5.0
```

The workflow runs every check, builds and install-tests both packages, and only then makes a **draft** release with the four packages, `SHA256SUMS` and a build-provenance attestation for each package. A draft is not public: download its packages, try them, and publish it on GitHub when they are good. A tag that does not match the version in `Cargo.toml` makes no release.

## Known limitations

- The stored password is used only when ssh's prompt names exactly the profile's user and host. A `HostName` alias in `~/.ssh/config` or a ProxyJump hop makes ssh show another name, and then you type the password.
- A wrong stored password is tried once; then ssh asks in the terminal, as plain ssh does (three attempts in all). A sync has no terminal, so there a wrong stored password fails the sync.
- Transcripts hold what VTE holds: content shown only on the alternate screen (htop, vim) is not in them, and a crash loses the transcript in progress.
- Removing a stored password is local to that machine. Others keep theirs, and a later sync from one that still has it can bring it back.
- The text size is saved when the last window closes. If Moniker is killed, or the session ends without closing its windows, the previously saved size stays.
- A process that ignores the hang-up keeps running after its tab closes, and its terminal is kept until it exits.
- The Transcripts window reads a transcript whole to show or search it (off the UI thread), so a very large transcript, or searching a very large folder, takes time and memory. A transcript you renamed is no longer listed (the file stays where it is). If an export onto an existing file fails part-way, that file is left partly written.
- Moving the transcripts folder across file systems copies each file, then removes it. Moniker waits for the move to end before it quits, but a kill or a logout in the middle can leave a truncated copy in the new folder; the original is still whole in the old one.
- Preferences cannot go back to the system monospace font once a font is chosen; removing `font` from `gui-settings.json` does it.
- Some keyring calls still run on the UI thread (opening the Sync page, saving a sync key, connecting, saving or removing a password on the profile page, deleting a profile, creating a profile with a password), so with a locked keyring the window can freeze briefly. A sync reads the keyring on its own thread.
- A sync's `ssh` has no `BatchMode`, so when you start `moniker` from a shell, a key-passphrase, ProxyJump or password prompt for a sync (the automatic one at launch too) can appear in that shell's terminal.
- Sync needs a Secret Service on the other machine; there is no fallback store.
- Not there yet: split panes, search in the terminal and color settings (the terminal colors follow the system light or dark style). The packages are for x86-64 only.

## License

Copyright © 2026 Justin M. Sloan. Released under the [MIT License](LICENSE).

The binaries include Rust crates (gtk4-rs, vte4, keyring, clap, argon2, chacha20poly1305 and their dependencies) under their own permissive licenses, MIT or Apache-2.0 for almost all of them. GTK, libadwaita and VTE are not bundled; they come from your system, under the LGPL.
