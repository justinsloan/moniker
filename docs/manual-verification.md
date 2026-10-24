# Hands-on verification checklist

Everything automatable is covered by `cargo test --workspace`, the window tests under `scripts/gui-tests.sh`, and `scripts/e2e-sync.sh`. The items below need a real desktop with a real pointer and keyboard, a real keyring or a real server: the window tests use a virtual display and synthetic signals. Build first with `cargo build --workspace` so `moniker-askpass` sits next to `moniker`, then start `target/debug/moniker`. Where a check needs a throwaway profile, config or data directory, start Moniker with `XDG_CONFIG_HOME=$(mktemp -d)` and `XDG_DATA_HOME=$(mktemp -d)` instead of your own.

## Terminal quality (the dealbreaker)

1. In a connected tab, `htop` and `tmux` render correctly, resizing works, and `seq 1 200000` scrolls without lag. They feel as fast as in your old terminal.
2. Scrollback in an ordinary tab holds about 10,000 lines (the Scrollback preference's default).
3. Switch the system style between light and dark (GNOME Settings, Appearance): open terminals recolor at once, and new ones start in the matching palette.
4. In light mode, white and bright white text on a black and on a blue background is readable (for example `printf '\e[37;40m a \e[97;40m b \e[37;44m c \e[97;44m d \e[0m'`); the light palette's neutrals were flagged, and white and bright white are now light in both palettes. Dark mode uses GNOME Console's own colors: it should look like Console.

## Launch, tabs and windows

5. Launch: one window with one local terminal in your home directory, and no tab bar. The title is the shell's title.
6. ＋ and Ctrl+Shift+T open a tab showing the new-tab list with Local terminal selected and focused: Enter gives a local shell in that same tab. Closing the tab before choosing just closes it.
7. In the new-tab list, type a filter and press Enter immediately: the profile that connects is the one shown (a stale-filter race was flagged).
8. Right-click (and long-press) on a tab opens its menu with Rename…; double-click on a tab opens Rename. A remote tab keeps its name when the remote shell sets a title (try `printf '\e]0;evil\a'`); a local tab follows its shell's title until renamed.
9. Ctrl+Shift+W while the Rename dialog is open does not close the tab behind it (the tab and terminal shortcuts are switched off while any dialog is open; they work again when it closes).
10. Ctrl+Shift+T/W/N and Ctrl+PgUp/PgDn work with htop, tmux and mc focused. F10 and Ctrl+Tab reach the terminal program (F10 is Quit in htop and mc, and must not open the ☰ menu).
11. ☰ → New Window opens a window with one local tab. Drag a tab out of the bar to make its own window, and drag it into another Moniker window and back. Drag the last tab of a window into another window (the emptied window closes), and drag a tab onto the desktop.
12. Drag a tab that is saving a transcript to another window and end its session there: one transcript, in the transcripts folder, written once.
13. Closing the last tab closes that window; closing the last window quits the app and the Connection Profiles and Transcripts windows with it.

## Connecting

14. Save a profile with a password, open a new tab (＋) and click the profile in the list (or arrow to it and press Enter): it logs in without typing. A profile with no stored password prompts in the terminal and allows ssh's usual three typed attempts (`Permission denied, please try again.` after a wrong one). Once such a tab has answered its password prompt from the keyring, `$XDG_RUNTIME_DIR/moniker/askpass` (mode 0700, as is its parent) holds an empty 0600 file named by that tab's `MONIKER_ASKPASS_TOKEN` (see `/proc/<ssh pid>/environ`); it is gone once the tab's ssh has exited, including after closing the tab. Key or agent logins make none; markers left by a quit or a crash stay until logout.
15. The first connection to a new host shows the host-key "yes/no" question in the terminal. It is never answered for you.
16. While a session is open, `ps aux | grep -E 'ssh|askpass'` shows no password in any command line, and `/proc/<ssh pid>/environ` holds only `SSH_ASKPASS`, `SSH_ASKPASS_REQUIRE=force`, `MONIKER_PROFILE_ID` and `MONIKER_ASKPASS_TOKEN` (a random UUID, not a secret).
17. The new-tab list (＋): Local terminal is selected and has the focus, typing anywhere goes to the search entry, the arrow keys move through the profiles, Right arrow and clicking › open that profile's choices (Just connect, each sequence, the "Save transcript" box set to the profile's default), and the sizing and animation look right. The › on Local terminal shows Open and the same box. A sequence runs its commands and leaves you in an interactive login shell; `exit` closes the tab.
18. A wrong stored password is tried once: ssh prints `Permission denied, please try again.` and then asks for the password in the terminal (input hidden). Typing the right one logs in. The stored password is not sent again: two more wrong typed attempts end the session (`Permission denied`, tab stays with `[session ended: status 255]`), and nothing loops. On a first connection to a new host, the host-key question comes first and the stored password still logs in by itself after `yes`.
19. If the server drops the connection, the tab stays with the status note.
20. With no Secret Service reachable, connecting still works and ssh asks for the password in the terminal, with ssh's usual three attempts.
21. If `moniker-askpass` is missing next to the GUI, a notification says so and ssh asks for the password in the terminal.

## The Connection Profiles window

22. ☰ → Connection Profiles opens a separate window in the GNOME Settings layout. On a fresh install (start `moniker` with `XDG_CONFIG_HOME` pointing at an empty directory) the sidebar updates after the first profile is saved, and after the file is changed from outside (`moniker-server` in a terminal, or a sync).
23. ＋ opens the New Profile dialog; Create saves the profile (with its password) and selects it.
24. Make the window narrow: the sidebar and the page collapse to one pane with a back button, and opening the window narrow does not jump into the first profile's page on its own.
25. Type in a profile field: a ✓ appears, and the value is saved only on ✓ or Enter, with a real pointer and keyboard. Typed text that is not applied is discarded when the field loses focus, another profile is selected or the window closes. A bad value (empty host) marks the row and a notification says why; nothing is saved.
26. The password field: empty means "keep the stored password"; after ✓ it clears. "Remove stored password (this machine only)" asks first.
27. A startup sequence: open one from the profile page, edit, Save; Cancel and Escape in the commands editor discard what was typed; Move Up / Move Down change the order of the Connect choices.
28. Double-click or Enter on a sidebar row, and the page's Connect button, open a session in the main window you used last and raise it; the Connection Profiles window stays open.
29. Delete Profile asks first, naming the profile; a name containing markup (for example `<b>x</b>`) is shown literally there, in the sidebar, the page title and the tab.

## Transcripts

30. With Save transcript on (the profile's switch), type a few commands and close the tab: a mode-0600 `<host>_<date>-<time>.txt` appears under `~/.local/share/moniker/transcripts/` with the session text and no escape sequences. `exit` in the remote shell closes the tab and writes it; closing the window with two tabs that save transcripts open writes both.
31. The connect-time "Save transcript" box overrides the profile setting in both directions.

## Sync (two real machines A and B)

32. Install Moniker on both so `moniker-server` is on the remote's non-interactive PATH (check with `ssh B which moniker-server`).
33. Set the same sync key (at least 12 characters) on both: the Sync page of the Connection Profiles window on A, `moniker-server sync-key set` on B. The key never appears on screen. A key shorter than 12 characters is refused with a clear message.
34. Add a profile with a stored password on A, add a profile for B on A, connect to B once from a tab to accept its host key, then choose B on the Sync page and press Sync. Expect "Syncing…", a responsive window, then "Synced: 1 added, …". The profile and its password arrive on B. Syncing again says "Already up to date.". ☰ now offers Sync Now.
35. Edit on one side and sync (the newest edit wins). Delete on one side and sync (it disappears on the other). Open windows follow: the sidebar and the new-tab list refresh.
36. Wrong key (change it on B only): "The other machine has a different sync key." and nothing changes on either side.
37. A host whose key was never accepted: "Connect to this host once from a tab…".
38. A remote with no Secret Service, or without `moniker-server` on its PATH, or with no sync key set: each gives a message that names the cause, and nothing is applied.
39. Make B unreachable during a sync: after at most two minutes, "the other machine did not finish in time…".
40. Leave the Sync page while a sync runs: the app stays responsive and the profile list refreshes when it finishes.
41. Automatic sync: quit and start Moniker again. The local terminal appears and works at once, and a successful sync is silent. With B unreachable there is one notification. The switch on the Sync page turns it off.
42. Automatic sync at launch with the keyring locked: the terminal stays responsive, the system's unlock prompt may appear, and canceling it gives one notification on failure.
43. Sync Now (☰) reports the result in a notification; pressed while a sync runs it says "A sync is already running.".
44. Start `moniker` from a terminal with a sync partner whose key has a passphrase: note whether a prompt appears in that terminal (a known open item: the sync's ssh has no `BatchMode`).

## Known behaviors to confirm you are happy with

45. Some keyring calls (opening the Sync page, saving a sync key, connecting) run on the GTK main thread, so a locked keyring can freeze the window briefly while GNOME asks for the unlock.
46. "Remove stored password (this machine only)" does not propagate through sync, and the password can come back from a machine that still has it.
47. `scripts/e2e-sync.sh` fails its "moniker-server not on remote PATH" check while `moniker-server` is installed in `/usr/bin`. That is the environment, not a regression.

## Alpha 3: copy and paste

48. Select text in a terminal with the mouse and press Ctrl+Shift+C, then paste it into another application: it is there as plain text. Ctrl+Shift+V pastes the clipboard into the terminal. Ctrl+C still interrupts (`sleep 100`, then Ctrl+C).
49. Ctrl+Shift+C with nothing selected while `cat -v` runs: nothing happens (no `^C`, `cat` keeps running).
50. Right-click in a terminal at a shell prompt: a menu with Copy and Paste. Copy is insensitive with no selection and sensitive with one; both work from the menu.
51. Right-click in `htop`, `mc` or `tmux` with `set -g mouse on` (mouse reporting): a plain right-click goes to the program and no Moniker menu appears; Shift+right-click opens the Copy / Paste menu.
52. Shift+F10 and the Menu key in `mc` and `htop`: note whether they reach the program or open Moniker's terminal menu (VTE may bind them to its context menu; an open question, not a requirement either way).
53. Paste several lines (for example three `echo` commands) at a bash prompt: they arrive as one block and nothing runs until you press Enter (bracketed paste), not line by line. In a program without bracketed paste (plain `cat`) the lines arrive as if typed.
54. Open the Rename dialog (double-click a tab) and press Ctrl+Shift+V: the text goes into the dialog's field, and nothing reaches the terminal behind it. The same dialog open, Ctrl+plus, Ctrl+minus and Ctrl+Shift+W do nothing to the tab behind either; also with ☰ → About Moniker, ☰ → Keyboard Shortcuts and ☰ → Preferences open. When the dialog closes, all of them work again.
55. Middle-click pastes the primary selection into a terminal.
56. On a tab that still shows the new-tab list (no terminal yet), Ctrl+plus, Ctrl+minus and Ctrl+0 do nothing, and typing and Ctrl+V in its search entry work as usual.

## Alpha 3: text size

57. Ctrl+plus, Ctrl+minus and Ctrl+0 with real keys: the text gets larger, smaller and back to normal, stopping at 50% and 300%. On a US layout Ctrl+plus needs Shift (Ctrl+= works without); the keypad + and − work too.
58. ☰ shows a row "− 100% +". The percent follows the size as it changes and as you switch tabs; clicking the percent resets the tab to 100%; the three buttons are insensitive on a tab that shows the new-tab list.
59. Each tab has its own size: zoom one tab and the other keeps its size. A new tab (＋, then Local terminal) starts at the size of the tab you were in.
60. `htop` and `tmux` redraw correctly right after zooming in and out, and after resizing the window at a zoomed size.
61. ☰ → Keyboard Shortcuts: the Terminal section lists Copy, Paste, Larger Text, Smaller Text and Normal Text Size, and the lines that have more than one key look sensible.
62. Set a tab to about 150% and close the window, then start Moniker again: the first tab starts at 150%. `~/.config/moniker/gui-settings.json` (mode 0600) holds `font_scale`. With two windows at different sizes, the one closed last decides.
63. Open three tabs and close them one by one until the window closes (the last one at, say, 170%), then relaunch: the first tab has the last tab's size.
64. The trade-off: in bash, Ctrl+minus zooms out instead of acting as readline's undo; Ctrl+_ still undoes.

## Alpha 3: the Transcripts window (Recorded Sessions until alpha 4)

Make a few transcripts first: connect to a profile with "Save transcript" on (or checked in its › choices), run something, and close the tab. For the odd-file checks use a throwaway data directory (`XDG_DATA_HOME=$(mktemp -d) target/debug/moniker`) rather than your real transcripts.

65. ☰ → Transcripts opens a window in the Connection Profiles window's layout. The sidebar lists the transcripts under day headings (Today, Yesterday, then dates), each row with its host and local start time, and the window warns that transcripts may include secrets.
66. The order choice flips Newest first / Oldest first. The host filter lists "All hosts" and each host that has transcripts, and narrows the list to one.
67. Typing in the sidebar search keeps only the transcripts that contain the text, each with its match count, across all of them (try a word that is in one transcript only); the host filter and order still apply; clearing the search shows everything again.
68. Choosing a transcript shows its whole text, read-only, in a monospace font. Its search bar highlights every match; Enter or ↓ goes to the next, ↑ to the previous; "N of M" is right. Ctrl+F focuses it. While the sidebar is searched, opening a transcript carries the search over and jumps to its first match.
69. Ctrl+F in a main-window terminal still reaches the program (at a bash prompt, type `abc`, press Left twice, then Ctrl+F: the cursor moves right); only the Transcripts window takes it.
70. Finish a session that saves a transcript while the window is open: the new transcript appears in the list without reopening.
71. Export…: the file dialog opens with the transcript's name. Saving into a folder gives a notification and a copy whose mode is 0600 (`stat -c %a`). Choosing an existing file replaces it (the file dialog asks first), and it is 0600 afterwards. Exporting onto the transcript itself (the transcripts folder, the same name) is refused with a message and the original is intact.
72. Delete asks first, naming the host and time as plain text. Cancel keeps the file. Delete removes it, empties the viewer and removes the row.
73. Make the window narrow: one pane at a time, with a back button.
74. A very large transcript (in a tab that saves a transcript run `seq 1 2000000`, close the tab, open the transcript) and one with a single multi-megabyte line (`yes | head -c 5000000 | tr -d '\n'`): the window stays responsive while the text loads, and scrolling and searching work. Say how long it takes. (Run on 0.3.0: the single line took about 6 s; checks 105-108 repeat it for alpha 4.)
75. Odd files (in a throwaway data directory, in `moniker/transcripts/`): files that are not named like transcripts are not listed: `notes.txt`, `.bashrc`, `host_badstamp.txt`, and a big one (`truncate -s 1G big.iso`, which takes no disk space). None of them appears, none is offered as a host, and a search across all transcripts stays quick (they are not read). A symbolic link is not listed, even one named like a transcript. A file named `<b>x&amp;_20261017-120000.txt` is listed under the host `<b>x&amp;`, shown literally, not in bold. (Until alpha 4 a name that did not parse was listed under its file name with its modification time.)

## Alpha 3: sessions, license and the empty case

76. Close a tab that runs `vim` in a local shell, and close a tab with a live `ssh` session (also closing a whole window with such tabs): `ps` shows no `vim` or `ssh` left over from that tab or window.
77. ☰ → About Moniker shows version 0.4.0, "Copyright 2026 Justin M. Sloan" and the MIT license, and its website link opens https://github.com/justinsloan/moniker.
78. With no profiles (start Moniker with `XDG_CONFIG_HOME` pointing at an empty directory), the new-tab list shows only Local terminal; ☰ → Connection Profiles opens the empty Connection Profiles window, and the first profile saved there appears in an open new-tab list.

## Alpha 3: tab rename fixes

79. Rename fixes (`73da778`): double-click a tab: the Rename dialog opens and the window does not maximize. Right-click a tab, then Rename…: the cursor is in the name field and typing replaces the name.

## Alpha 4: Preferences

80. ☰ has Preferences (below Sync Now, above Keyboard Shortcuts). It opens a dialog with the groups Terminal (Font, Scrollback), Transcripts (with the warning that transcripts may hold secrets; Save transcripts of all sessions; Transcripts folder with Choose…) and Sync. Ctrl+comma opens it too, also from a second Moniker window (the open dialog is raised, never a second one), and ☰ → Keyboard Shortcuts lists Preferences under General. With the dialog open, Ctrl+Shift+T, Ctrl+Shift+W and the text size keys do nothing to the tab behind it.
81. Font: the font button opens the real GTK font chooser, which offers monospace fonts only (no Sans or Serif). Choosing another font changes every open terminal, in every window, at once; `htop` and `tmux` still render, resize and redraw correctly and as fast as before; a new tab uses it; Ctrl+plus still zooms. Quit and start again: the first tab uses the chosen font, and `~/.config/moniker/gui-settings.json` (mode 0600) holds `font`.
82. Scrollback: set 1,000, then open a new tab and run `seq 1 5000`: scrolling back reaches only about 1,000 lines, while a tab opened before still holds 10,000. A tab that saves a transcript keeps everything (`seq 1 20000`, close it, the transcript has every line). The row accepts only 1,000 to 1,000,000. Quit and start again: the value is kept.
83. Sync: the Open button of the Sync row closes Preferences and shows the Connection Profiles window on its Sync page.
84. A setting that cannot be saved: start Moniker with a throwaway `XDG_CONFIG_HOME`, put a broken file at `$XDG_CONFIG_HOME/moniker/gui-settings.json` (`mkdir -p` the folder, then `echo '{' >` the file), and change a Preferences row. A message appears in the dialog, at its bottom ("Could not save the preferences…"), the row goes back to what it was, and the file is left as it was.

## Alpha 4: saving transcripts

85. With a fresh config, Save transcripts of all sessions is off. Clicking Local terminal in the new-tab list and closing that tab saves nothing. The › on Local terminal shows Open and an unchecked "Save transcript" box; checking it and pressing Open saves a transcript when the tab closes.
86. Switch it on. A local tab (click Local terminal) now saves a transcript on close, and so does a connection to a profile whose own "Save transcript" switch is off; the box under › starts checked for both, and unchecking it before Open or Just connect saves nothing for that session while other tabs still do. A tab that was already open when the switch changed is not affected.
87. The launch tab: with the switch on, quit and start Moniker, type something in the first tab and close it: a transcript is saved. Switch it off, start again: the first tab saves nothing.
88. A local transcript is named `<this computer's host name>_<date>-<time>.txt` (compare `hostname`). In the Transcripts window it is listed under that host, which the host filter offers, with the connection transcripts under theirs.
89. Wording: the ☰ menu says Connection Profiles, Transcripts and Preferences; a profile's page has a "Save transcript" switch; the › choices have a "Save transcript" box; the windows are titled Connection Profiles and Transcripts; and "Recorded Sessions" and "Record transcript" appear nowhere. In the narrowest layout the Connection Profiles and Transcripts sidebar titles are not cut off badly.

## Alpha 4: the transcripts folder

Use throwaway folders (make them in a file manager or with `mkdir`) and a throwaway `XDG_DATA_HOME`, with a few transcripts in the default folder.

90. Choose… opens the real folder chooser, starting at the current folder. Choose a new empty folder: the row shows its path, a notification says "Moved N transcripts.", all of them are in the new folder (mode 0600) and none in the old one. Choose… is insensitive while the move runs. A Transcripts window that was open lists the new folder.
91. A taken name: before choosing, put a file with the same name as one of the transcripts in the new folder. After the move that transcript is there as `…-2.txt` and the existing file is untouched.
92. Other files: put `notes.txt` and a symbolic link in the old folder. They stay where they are, and the message adds "1 other file was left in the old folder." and ends with "Old folder: " and the old folder's path (the link is not counted).
93. Across file systems, if one is at hand (a USB stick, or `/tmp` if it is a tmpfs; `df` shows which): the move copies and then removes, every file arrives whole (`cmp` with a copy you made beforehand) and is mode 0600.
94. Refusals: choose a folder that cannot be written (`/`, or a read-only mount): a message in the dialog says why and nothing changes. Choosing the folder already in use does nothing.
95. A session open during the change: with a tab that saves a transcript open, change the folder and then close that tab: its transcript is in the new folder and nothing new is in the old one.
96. A transcript saved after the change goes to the new folder. Quit and start again: the folder is remembered and the Transcripts window lists it. Choosing the default folder (`~/.local/share/moniker/transcripts`) again moves them back the same way.

## Alpha 4: notifications

97. Look: cause one (an empty host on a profile page, or ☰ → Sync Now with a sync partner set). It appears at the top center, just below the header bar, as a dark rounded pill over the terminal or the page, and fades after about 8 s. It stays readable in the light, dark and high contrast styles.
98. A long message (a very long profile name in a rejected field, say) wraps onto several lines within a comfortable width and stays a little longer, up to 15 s. Several in a row (press Sync Now twice quickly, or reject two fields) appear one after another and never overlap.
99. The pill covers what is under it and cannot be dismissed: on a new tab it covers the search entry, and in the Transcripts viewer its search bar, for 8 to 15 s. Say whether that is acceptable.
100. A message that arrives while Rename or About is open is shown after the dialog closes, in full.
101. A message about Preferences (check 84, or a refused folder) shows in the dialog at its bottom; once the dialog is closed, messages show at the top of the window again. Then a move that ends as the dialog closes: put a few thousand transcripts in the transcripts folder (in it, `echo x > web_20261017-120000.txt; for i in $(seq 2 3000); do cp web_20261017-120000.txt web_20261017-120000-$i.txt; done`), choose a new folder and close the dialog at once (Escape). The "Moved N transcripts." message is still seen, in the dialog if it was still open or at the top of the window after.
102. The side windows (Connection Profiles, Transcripts): a notification appears just below their header bars, also with larger text (GNOME Settings, Accessibility, Large Text): the pill is still below the header bar and not clipped.
103. With Orca running, a notification is read out.

## Alpha 4: transcripts of one huge line

Use the release build (`cargo build --release --workspace`, then `target/release/moniker`) for the timing.

104. Make the transcript: in a tab that saves a transcript run `yes | head -c 5000000 | tr -d '\n'` and close the tab (or use your own 5 MB one-line file in the transcripts folder).
105. Open it in the Transcripts window: the text appears in about a second (say how long; on 0.3.0 it was about 6 s) and the window stays responsive. The text is shown in lines of 2,000 characters.
106. Search in it with Ctrl+F in the viewer: matches are found and highlighted, one that straddles a line break (the last characters of a 2,000-character line and the first of the next) included, and "N of M" is right.
107. Export it and compare the copy with the original in the transcripts folder (`cmp`): identical; the display breaks are not in it.
108. Select across one of those line breaks and copy it (Ctrl+C, and again with Copy in the right-click menu), then paste into a text editor or a terminal: the text is the original, with no added newline (a line longer than 2,000 characters, pasted into bash, stays one line). A middle-click paste of the same selection still has the added newline (expected; Export gives the exact text). A normal transcript, and the 2,000,000-line one from check 74, open no slower than on 0.3.0.

## Alpha 4: packaging

109. `bash scripts/build-deb.sh` makes `target/debian/moniker_0.4.0_amd64.deb` and `moniker-server_0.4.0_amd64.deb`. `dpkg-deb -f` of each shows `Maintainer: Justin Sloan <your address>` and `Homepage: https://github.com/justinsloan/moniker` and, for `moniker`, `Depends` with `libadwaita-1-0 (>= 1.9)` and `libvte-2.91-gtk4-0 (>= 0.76)`; `dpkg-deb -c` of `moniker` lists `/usr/share/metainfo/org.moniker.Moniker.metainfo.xml`.
110. After installing the packages yourself: open Moniker's page in App Center (or GNOME Software, Installed): the publisher or developer reads "Justin Sloan". `apt show moniker` shows the Maintainer as Justin Sloan. Opening the .deb file itself in App Center shows only the package name and no publisher: that is App Center's own page and no package can change it.

## Alpha 5: going back to the system font

111. In Preferences, with no font chosen (a fresh config), the Font row shows the system monospace font and no reset button. Choose another font: a reset button (an undo arrow, tooltip "Use the System Font") appears beside the font button. Press it: every open terminal, in every window, goes back to the system monospace font at once, a new tab uses it, the font button shows it again, the reset button goes away and the keyboard focus is on the font button. In `~/.config/moniker/gui-settings.json`, `font` is `null`, and after a restart the terminals still use the system font.

## Alpha 5: stored passwords through `~/.ssh/config`

112. Make a profile whose host is an alias in `~/.ssh/config` (`Host myalias` with `HostName` the real server, and your user name and stored password in the profile). Connecting logs in by itself, as for a profile with the real host name. The same with `HostKeyAlias` set for that host.
113. A profile for a host reached through `ProxyJump` (a jump host that asks for a password): the jump host's password prompt appears in the terminal and you type it; the target's prompt then gets the stored password by itself. The stored password never answers the jump host's prompt, even when you make the jump host's name the same as the target's (for example `HostKeyAlias` set to the jump host's name).
114. Break the lookup: put `Match exec "sleep 10"` (with nothing under it) at the top of `~/.ssh/config`, then connect to the alias profile of check 112. ssh itself takes about 10 s to run the command; then the helper's lookup gives up after 5 s and the terminal asks for the password (the profile's own host, the alias, is expected and does not match the real host in the prompt). Remove the line afterwards.

## Alpha 5: a sync never asks in a terminal

115. Start `moniker` from a shell in a terminal window, with a sync partner whose login needs something typed (a key with a passphrase that is not in `ssh-agent`, or a password that is not stored; check this with ☰ → Sync Now and with the automatic sync at launch). Nothing appears in that shell's terminal; the sync fails within a few seconds with a notification ending "A sync cannot ask for a password or a key passphrase: store the profile's password, or add the key to ssh-agent." With the password stored (or the key added to the agent) the sync works as before. `moniker-server sync <profile>` in a terminal still asks for the passphrase there.

## Alpha 5: a locked keyring freezes nothing

116. Lock the login keyring (Passwords and Keys, right-click Login, Lock), with `htop` running in one tab. Connect to a profile that has a stored password: the new tab is named after the profile and, after a moment, says "Waiting for the keyring…" with a spinner; the unlock prompt appears; `htop` keeps redrawing and the other tabs and windows take input while the prompt is up. Unlock: the tab connects and logs in by itself. Lock it again, connect, switch to another tab before unlocking and type there: after unlocking, your keys stay in that tab (the new session does not take the keyboard) until you go back to the session's tab. Lock it again, connect, and close the waiting tab before unlocking: after unlocking, no session starts (`ps` shows no new `ssh`).
117. With the keyring locked, and the prompt dismissed each time (Cancel): open ☰ → Connection Profiles → Sync (the line says "Sync key: checking…" until the keyring answers, then the reason it could not be read); save a sync key; save a password on a profile's page; remove a stored password; create a profile with a password; delete a profile. Each says why it failed, also when you moved on to another profile (or left the Sync page) before cancelling, and the window never freezes. While the Sync page reads the keyring it shows no "Set a sync key first." hint, and Sync stays off while a new key is being saved. Then unlock and repeat each: they work, and a deleted profile's password is gone from the keyring too (Passwords and Keys).

## Alpha 5: which hosts answer

Watching the pings: `sudo tcpdump -ni any icmp or icmp6` in another terminal.

118. In the new-tab list and in the Connection Profiles sidebar, each profile has a small mark before its name: a green dot for a host on your network that is up, a red ring for one that is off (after about three seconds, or six for a name with both an IPv6 and an IPv4 address), and a short gray dash until a check has decided. Hovering a mark says "Answers ping", "No reply to ping" or "Not checked yet". Local terminal and the Sync entry have none. The marks are subtle, look the same in the light, dark and high-contrast styles, are not clickable, and do not move; the dot, the ring and the dash can be told apart without color.
119. A profile whose host is reached through `ProxyJump` or `ProxyCommand` in `~/.ssh/config` keeps a gray dash with "Not checked: reached through another host", and tcpdump shows no ping for it. A profile whose host is an alias (`Host nas` with `HostName 192.168.1.20`) shows the status of 192.168.1.20. With a `Match exec "sleep 10"` at the top of `~/.ssh/config`, a profile's mark stays a gray dash with "Not checked: could not read the ssh configuration" and nothing is pinged for it; a `Match exec "date >> /tmp/match.log"` shows the command running at most once every five minutes per host while a list is open (remove both afterwards).
120. One status per host: two profiles on the same host show the same mark; opening several new tabs within half a minute sends no new pings (tcpdump); with a list open, the hosts are pinged again about once a minute; with only terminals open, nothing is sent. The requests carry only zeros (`tcpdump -X`).
121. Preferences, Connection Profiles, "Show whether hosts are reachable" (on to start with): turned off, every mark disappears at once and tcpdump shows no ping, even with a list open, and not even one more from a check that was under way (turn it off just as a list opens with a host that is down); turned on again, the marks come back gray and then decide. The setting survives a restart (`ping_hosts` in `~/.config/moniker/gui-settings.json`). With Large Text on (GNOME Settings, Accessibility) the marks grow with the text.
122. With Orca running, moving through the sidebar reads each mark's words.
123. A profile whose host name does not exist (`no-such-host.invalid`): its mark is a red ring at once, hovering it says "Name not found", and tcpdump shows no ping for it.
124. A host whose name has both an IPv6 and an IPv4 address, on a network with IPv6: tcpdump shows ICMPv6 requests to it, and if its IPv6 address gets no reply, ICMP (IPv4) requests three seconds later; the mark is green if either answers. With `AddressFamily inet` for that host in `~/.ssh/config` (then the marks turned off and on in Preferences), only IPv4 requests are sent; with `AddressFamily inet6`, only IPv6 ones (remove it afterwards).
125. With a list open and `Match exec "date >> /tmp/match.log"` at the top of `~/.ssh/config`, turn the marks off and on in Preferences: a new line appears in `/tmp/match.log` at once, not up to five minutes later. Change a profile's host's `HostName` in `~/.ssh/config` and turn the marks off and on: the mark shows the new host's status at once (remove both afterwards).

## Alpha 5: saving never freezes the window

The profile file busy, as a sync that waits for the keyring holds it: `flock ~/.config/moniker/profiles.json.lock sleep 30` in a terminal, with `htop` running in a Moniker tab.

126. While `flock` runs, in the Connection Profiles window: change a profile's name and press Enter, change its port, flip Save transcript, and clear its key file. The window and `htop` keep going; each row keeps what you set and, after a moment, shows a small spinner; the name row keeps the keyboard focus but takes no typing until it is saved. When `flock` ends, all four are saved (the spinners go, the sidebar shows the new name), and a restart shows them.
127. While `flock` runs, flip Save transcript on and then off again: once it ends, the switch and the file both say off. Open a startup sequence, edit it and click Save: the sub-page stays with a spinner in its header, takes no typing and no second Save, and closes once `flock` ends, with the edit saved.
128. While `flock` runs, type an invalid host (`bad host!`), press Enter, and select another profile: when `flock` ends, a notification says what is wrong with the host, and nothing was saved. Without `flock`, saving works as before, with no spinner and no flicker.

## Alpha 5: panes

In a window with a local shell, a connection to a saved profile, and `htop` running in one of them.

129. Ctrl+Shift+R, then pick a profile in the new pane's list: two panes side by side, each with a header (the pane you are in has a line under its title, the other's title is dimmed). Ctrl+Shift+D in the right one, then pick Local terminal: the right column splits top and bottom. Drag the dividers: the panes resize, and `htop` redraws to its new size and stays fast. Make the window full screen and back: the proportions stay.
130. Ctrl+Shift+arrows go from pane to pane (left from a pane on the right lands on the left one; up and down stay in the column); clicking a pane goes there too. The tab and the window title follow the pane you are in, its tooltip lists all three, and the ☰ menu's text size and Ctrl+plus act on that pane alone. In a tab with one pane, Ctrl+Shift+Up and Down still scroll one line.
131. Right-click a pane you are not in: Paste pastes there, and Split Right splits that pane. Double-click a pane's title: "Rename Pane", and the new name shows in its header and, while you are in it, on the tab.
132. Drag a pane by its header onto the left part of another pane: a highlight shows the left half, and the pane lands there. Drag one onto a tab of the tab bar (the bar shows while you drag, even with one tab): it becomes a tab of its own. Hover a tab while dragging until it opens, and drop into one of its panes. Drag one outside the window: a window of its own. The session keeps running each time (type in it), and a pane alone again loses its header.
133. Open a second tab and drag it (from the tab bar) onto the bottom of a pane of the first tab: its panes go below that pane, laid out as they were, and the second tab goes; no extra window appears. Drag a tab onto another window's tab bar: it moves there as a tab, as before.
134. Close a pane with its × and with Ctrl+Shift+W: the pane beside it takes the room and the focus, and its session is hung up (in a pane that saves a transcript, the transcript is written). `exit` in a pane closes that pane only; the tab's own × closes every pane; the last pane closed closes the tab.
