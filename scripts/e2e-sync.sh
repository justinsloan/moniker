#!/usr/bin/env bash
# End-to-end check that two Moniker instances sync profiles and passwords over
# SSH. Run it from anywhere; it builds what it needs first.
#
# How it works
#   The script re-runs itself ("inner" phase) under `env -i` inside its own
#   private D-Bus session, with a private gnome-keyring-daemon (Secret Service)
#   that it creates and unlocks itself, and throwaway user-level sshd servers
#   on 127.0.0.1 (key login; key login without `moniker-server` on PATH; password
#   login). Instance A is the script's own side, instance B is whatever
#   `moniker-server sync-serve` runs inside the sshd session. Passwords and sync keys
#   are passed only on stdin or through files, never argv or the environment,
#   and are never printed.
#
# What it touches (nothing else)
#   * One temp dir /tmp/moniker-e2e.XXXXXXXX ($T): configs, keys, the private
#     bus socket, the private keyring, logs. Removed on exit.
#   * The real ~/.ssh/known_hosts: `moniker-server sync` pins StrictHostKeyChecking=yes
#     and ssh reads that file from the passwd entry, so the throwaway host key
#     has to be in it for the sync checks. It is backed up first, one line is
#     appended per throwaway sshd, and it is restored byte-identical on exit
#     (or removed, if the script created it). Never `ssh-keygen -R`.
#   * Local TCP ports 20000-29999 on 127.0.0.1.
#   It never uses the real keyring, the real session bus or the real display.
#   Before and after, it records `stat` of /run/user/UID/at-spi, /run/user/UID/bus
#   and ~/.local/share/keyrings/login.keyring (and that no known_hosts.old
#   appears) and fails the run if any changed.
#
# Needs: bash, cargo, sshd (/usr/sbin/sshd), ssh, ssh-keygen, dbus-run-session,
#   dbus-daemon, gnome-keyring-daemon, busctl, ss, setsid, timeout.
#
# Exit status: 0 every check passed; 1 a check failed or the real session or
#   known_hosts changed; 3 refused to run (no safe private setup possible).

set -u
set -o pipefail

SELF=$(readlink -f "${BASH_SOURCE[0]}")
REPO=$(cd "$(dirname "$SELF")/.." && pwd)
SYSTEM_PATH=/usr/bin:/bin
T=    # private temp dir, set once it exists
KH=   # the real ~/.ssh/known_hosts, set once it is known
BIN=  # directory with the built moniker binaries

say() { printf '%s\n' "$*"; }
warn() { printf '%s\n' "$*" >&2; }
refuse() { warn "e2e-sync: refusing to run: $*"; exit 3; }

# Paths of the known_hosts bookkeeping, all inside $T (needs $T).
set_kh_paths() {
    KH_ORIG=$T/kh.orig       # byte-for-byte copy of the original file
    KH_EXISTED=$T/kh.existed # exists only if the file existed originally
    KH_MINE=$T/kh.mine       # exactly the lines this script appended
}

# Put known_hosts back as it was. Safe to call more than once and from both
# phases. Returns non-zero if it could not.
restore_known_hosts() {
    if [ -f "$KH_EXISTED" ]; then
        cmp -s -- "$KH" "$KH_ORIG" && return 0
        cp -p -- "$KH_ORIG" "$KH" && cmp -s -- "$KH" "$KH_ORIG"
        return
    fi
    # It did not exist before: remove it only if it is exactly what this script
    # wrote (and only if this script wrote anything at all).
    [ -e "$KH" ] && [ -s "$KH_MINE" ] || return 0
    if cmp -s -- "$KH" "$KH_MINE"; then
        rm -f -- "$KH"
    else
        warn "e2e-sync: $KH was changed during the run; left in place"
        return 1
    fi
}

# Start time of a process (clock ticks since boot, field 22 of /proc/PID/stat).
# With the PID it identifies one process for good, even if the PID is reused.
proc_start() {
    local stat
    stat=$(cat "/proc/$1/stat" 2>/dev/null) || return 1
    stat=${stat##*) } # drop "PID (command) ", which may contain spaces
    # shellcheck disable=SC2086
    set -- $stat
    printf '%s' "${20}"
}

# Last resort for the outer phase: stop the processes the inner phase recorded
# in $T/pids, but only those still running with the recorded start time.
kill_stragglers() {
    local pid start sig sent
    [ -f "$T/pids" ] || return 0
    for sig in TERM KILL; do
        sent=0
        while read -r pid start; do
            [ -n "$start" ] && [ "$(proc_start "$pid")" = "$start" ] || continue
            kill "-$sig" "$pid" 2>/dev/null && sent=1
        done <"$T/pids"
        [ "$sent" -eq 0 ] && return 0
        sleep 0.5
    done
}

# ======================================================================
# Outer phase: normal environment. Builds, makes $T, then re-runs the script
# inner phase under env -i and checks the real session afterwards.
# ======================================================================

need_tools() {
    local tool missing=0
    for tool in cargo ssh ssh-keygen dbus-run-session dbus-daemon gnome-keyring-daemon \
        busctl ss setsid timeout getent od cmp; do
        command -v "$tool" >/dev/null 2>&1 || { warn "missing tool: $tool"; missing=1; }
    done
    [ -x /usr/sbin/sshd ] || { warn "missing tool: /usr/sbin/sshd"; missing=1; }
    return "$missing"
}

build_binaries() {
    cd "$REPO" || return 1
    cargo build -q -p moniker-cli || return 1
    cargo build -q -p moniker-cli --examples || return 1
    BIN=$(readlink -f "${CARGO_TARGET_DIR:-$REPO/target}")/debug
    [ -x "$BIN/moniker-server" ] && [ -x "$BIN/moniker-askpass" ] && [ -x "$BIN/examples/e2e_helper" ]
}

# The private session bus: a socket in $T/run and no service directories, so
# nothing can be activated except what this script starts itself.
write_dbus_conf() {
    cat >"$T/dbus.conf" <<EOF
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <keep_umask/>
  <listen>unix:path=$T/run/bus</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
EOF
}

# Create $T and everything the inner phase needs in it, and install the exit
# trap before anything else can fail.
make_private_dir() {
    T=$(mktemp -d /tmp/moniker-e2e.XXXXXXXX) || refuse "cannot create a temp dir"
    set_kh_paths
    trap outer_cleanup EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM HUP
    case $T in *[[:space:]]*) refuse "temp dir path contains whitespace" ;; esac
    [ "$(readlink -f "$T")" = "$T" ] || refuse "temp dir is not a plain path: $T"
    [ "${#T}" -le 60 ] || refuse "temp dir path is too long for a socket: $T"
    case $T in /run/user/*) refuse "temp dir is under /run/user" ;; esac
    mkdir -m 700 "$T/run" "$T/home" "$T/data" || refuse "cannot create the private dirs"
    write_dbus_conf || refuse "cannot write the private bus config"
}

# The real known_hosts (ssh takes it from the passwd entry, not $HOME) and the
# copy of it to restore from.
snapshot_known_hosts() {
    local home_real
    home_real=$(getent passwd "$(id -un)" | cut -d: -f6)
    [ -d "$home_real/.ssh" ] || refuse "no $home_real/.ssh directory"
    KH=$home_real/.ssh/known_hosts
    : >"$KH_MINE"
    [ -e "$KH" ] || return 0
    [ -f "$KH" ] && [ -r "$KH" ] || refuse "$KH is not a readable file"
    cp -p -- "$KH" "$KH_ORIG" && : >"$KH_EXISTED" || refuse "cannot back up $KH"
}

# One line per real-session file that exists: name, inode, mtime, size.
stat_real_session() {
    local uid home_real f
    uid=$(id -u)
    home_real=$(getent passwd "$(id -un)" | cut -d: -f6)
    for f in "/run/user/$uid/at-spi" "/run/user/$uid/bus" \
        "$home_real/.local/share/keyrings/login.keyring" "$KH.old"; do
        if [ -e "$f" ]; then stat -c '%n %i %Y %s' -- "$f"; fi
    done
}

run_inner() {
    env -i PATH="$SYSTEM_PATH" HOME="$T/home" XDG_RUNTIME_DIR="$T/run" \
        XDG_CONFIG_HOME="$T/cfg-unset" XDG_DATA_HOME="$T/data" \
        MONIKER_E2E_INNER=1 E2E_T="$T" E2E_KH="$KH" E2E_BIN="$BIN" \
        "$(command -v dbus-run-session)" --config-file="$T/dbus.conf" -- "$BASH" "$SELF"
}

# Put known_hosts back and report; non-zero if it could not be restored.
report_known_hosts() {
    if restore_known_hosts; then
        say "PASS: known_hosts is as it was before the run ($( [ -f "$KH_EXISTED" ] && echo "byte-identical to the backup" || echo "absent, as before" ))"
    else
        say "FAIL: known_hosts could not be restored"
        return 1
    fi
}

# Compare the real session files with the snapshot taken before the run.
report_session_files() {
    stat_real_session >"$T/stat.after"
    if diff -u "$T/stat.before" "$T/stat.after" >&2; then
        say "PASS: real session files unchanged (at-spi, bus, login.keyring, no known_hosts.old)"
    else
        say "FAIL: a real session file changed during the run (see the diff above)"
        return 1
    fi
}

# Runs on every exit path. Signals are ignored meanwhile so that a second
# Ctrl-C cannot cut the clean-up short. known_hosts comes first: it is the one
# real file that is changed while the run lasts.
outer_cleanup() {
    local rc=$? ready=0
    trap - EXIT
    trap '' INT TERM HUP
    [ -n "$KH" ] && [ -f "$T/stat.before" ] && ready=1
    if [ "$ready" -eq 1 ]; then report_known_hosts || rc=1; fi
    kill_stragglers
    if [ "$ready" -eq 1 ]; then report_session_files || rc=1; fi
    case $T in /tmp/moniker-e2e.*) rm -rf -- "$T" ;; esac
    exit "$rc"
}

outer_main() {
    export PATH="$HOME/.cargo/bin:$PATH"
    need_tools || refuse "required tools are missing (see above)"
    build_binaries || refuse "cargo build failed"
    make_private_dir
    snapshot_known_hosts
    stat_real_session >"$T/stat.before"
    say "== Moniker end-to-end sync check (private environment $T)"
    run_inner
    exit $?
}

# ======================================================================
# Inner phase: env -i, private session bus. Everything below runs here.
# ======================================================================

declare -A PID_START # recorded PID -> its start time (see proc_start)
PASSED=0
FAILED=0
REASON=""
RC=0
ASKPASS_ID=
KEYRING_PW=moniker-e2e-throwaway # protects only the throwaway keyring in $T
# Display texts of moniker-core's SyncError::WrongSyncKey and ::ServerNotFound
# (crates/moniker-core/src/sync.rs), and the CLI's own "no sync key" message
# (crates/moniker-cli/src/main.rs, sync_serve); the CLI prints them on stderr.
MSG_NO_SYNC_KEY='no sync key is set on the remote machine (where `moniker-server sync-serve` runs); run `moniker-server sync-key set` there'
MSG_WRONG_KEY="the sync passphrases differ (or the data was corrupted on the way)"
MSG_NO_SERVER="moniker-server is not installed on the remote machine (or not on its PATH)"

# Refuse unless this really is the private setup.
assert_private_env() {
    local uid real
    uid=$(id -u)
    real=$(readlink -f "${XDG_RUNTIME_DIR:-/nonexistent}")
    case $T in /tmp/moniker-e2e.*) ;; *) refuse "E2E_T is not set up by the outer phase" ;; esac
    [ "$real" != "/run/user/$uid" ] || refuse "XDG_RUNTIME_DIR is the real /run/user/$uid"
    [ "${XDG_RUNTIME_DIR:-}" = "$T/run" ] || refuse "XDG_RUNTIME_DIR is not $T/run"
    [ "${XDG_DATA_HOME:-}" = "$T/data" ] || refuse "XDG_DATA_HOME is not $T/data"
    [ "${HOME:-}" = "$T/home" ] || refuse "HOME is not $T/home"
    case ${DBUS_SESSION_BUS_ADDRESS:-} in "unix:path=$T/run/bus"*) ;; *) refuse "not on the private session bus" ;; esac
    [ -z "${GNOME_KEYRING_CONTROL:-}" ] || refuse "GNOME_KEYRING_CONTROL is set"
    [ -S "$T/run/bus" ] || refuse "the private bus socket is missing"
}

# Record a background process this script started (`$!`) so that it can be
# stopped later, here and, if this phase dies hard, by the outer phase.
register_pid() {
    PID_START[$1]=$(proc_start "$1")
    printf '%s %s\n' "$1" "${PID_START[$1]}" >>"$T/pids"
}

# Stop a process this script started (by its recorded PID) and reap it. A PID
# that is gone, or now belongs to another process, is left alone.
stop_pid() {
    local pid=$1 i
    [ -n "${PID_START[$pid]:-}" ] && [ "$(proc_start "$pid")" = "${PID_START[$pid]}" ] || return 0
    kill "$pid" 2>/dev/null || return 0
    for i in $(seq 1 50); do
        kill -0 "$pid" 2>/dev/null || break
        sleep 0.1
    done
    kill -0 "$pid" 2>/dev/null && kill -KILL "$pid" 2>/dev/null
    { wait "$pid"; } 2>/dev/null
    return 0
}

# Runs on every exit path of the inner phase; same rules as outer_cleanup.
inner_cleanup() {
    local rc=$? pid
    trap - EXIT
    trap '' INT TERM HUP
    restore_known_hosts
    for pid in "${!PID_START[@]}"; do stop_pid "$pid"; done
    exit "$rc"
}

# ---------- private Secret Service ----------

bus() { busctl --address="unix:path=$T/run/bus" "$@"; }

wait_for() { # wait_for SECONDS COMMAND... : retry until it succeeds or time runs out
    local deadline=$((SECONDS + $1))
    shift
    until "$@"; do
        [ "$SECONDS" -lt "$deadline" ] || return 1
        sleep 0.25
    done
}

secrets_name_owned() { bus list --no-legend 2>/dev/null | grep -q '^org\.freedesktop\.secrets '; }

login_unlocked() {
    [ "$(bus get-property org.freedesktop.secrets /org/freedesktop/secrets/collection/login \
        org.freedesktop.Secret.Collection Locked 2>/dev/null)" = "b false" ]
}

unlock_and_check() {
    printf '%s' "$KEYRING_PW" | gnome-keyring-daemon --unlock >/dev/null 2>&1
    login_unlocked
}

start_keyring_daemon() {
    gnome-keyring-daemon --foreground --components=secrets >>"$T/gkd.log" 2>&1 &
    GKD_PID=$!
    register_pid "$GKD_PID"
}

# gnome-keyring creates the login keyring on the first --unlock but only serves
# it after a restart, so: create it with one daemon, then run a second one.
start_keyring() {
    start_keyring_daemon
    wait_for 30 secrets_name_owned || return 1
    printf '%s' "$KEYRING_PW" | gnome-keyring-daemon --unlock >/dev/null 2>&1
    wait_for 10 test -f "$XDG_DATA_HOME/keyrings/login.keyring" || return 1
    stop_pid "$GKD_PID"
    start_keyring_daemon
    wait_for 30 secrets_name_owned || return 1
    wait_for 30 unlock_and_check
}

# ---------- instances and servers ----------

ME=$(id -un)

A() { env XDG_CONFIG_HOME="$T/A/cfg" MONIKER_KEYRING_SERVICE=moniker-e2e-a "$@"; }
B() { env XDG_CONFIG_HOME="$T/B/cfg" MONIKER_KEYRING_SERVICE=moniker-e2e-b "$@"; }

random_token() { head -c 18 /dev/urandom | od -An -tx1 | tr -d ' \n'; }

free_port() {
    local p i
    for i in $(seq 1 200); do
        p=$((20000 + RANDOM % 10000))
        [ -z "$(ss -ltnH "sport = :$p")" ] && { printf '%s' "$p"; return 0; }
    done
    return 1
}

# sshd_config for one throwaway server. B's environment is set with SetEnv, so
# `moniker-server sync-serve` in the session is instance B on the private bus.
write_sshd_config() { # write_sshd_config FILE PORT NAME REMOTE_PATH key|password
    local auth="PasswordAuthentication no" pubkey="PubkeyAuthentication yes"
    [ "$5" = password ] && { auth="PasswordAuthentication yes"; pubkey="PubkeyAuthentication no"; }
    cat >"$1" <<EOF
Port $2
ListenAddress 127.0.0.1
HostKey $T/hostkey
AuthorizedKeysFile $T/authorized_keys
PidFile $T/sshd_$3.pid
$auth
$pubkey
KbdInteractiveAuthentication no
UsePAM no
PermitUserRC no
StrictModes no
LogLevel VERBOSE
SetEnv XDG_CONFIG_HOME=$T/B/cfg MONIKER_KEYRING_SERVICE=moniker-e2e-b DBUS_SESSION_BUS_ADDRESS=unix:path=$T/run/bus XDG_RUNTIME_DIR=$T/run XDG_DATA_HOME=$T/data HOME=$T/home PATH=$4
EOF
}

port_speaks_ssh() { # the port answers with an SSH banner
    local banner
    banner=$( (exec 3<>"/dev/tcp/127.0.0.1/$1" && read -r -t 2 line <&3 && printf '%s' "$line") 2>/dev/null)
    case $banner in SSH-2.0-*) return 0 ;; *) return 1 ;; esac
}

# Start a throwaway sshd; sets SSHD_PORT. Retries on another port if it fails.
start_sshd() { # start_sshd NAME REMOTE_PATH key|password
    local try port pid
    for try in 1 2 3; do
        port=$(free_port) || return 1
        write_sshd_config "$T/sshd_$1.conf" "$port" "$1" "$2" "$3"
        /usr/sbin/sshd -D -e -f "$T/sshd_$1.conf" >"$T/sshd_$1.log" 2>&1 &
        pid=$!
        register_pid "$pid"
        if wait_for 10 port_speaks_ssh "$port" && kill -0 "$pid" 2>/dev/null; then
            SSHD_PORT=$port
            return 0
        fi
        stop_pid "$pid"
    done
    warn "sshd '$1' did not start; log:"
    tail -n 5 "$T/sshd_$1.log" >&2
    return 1
}

# Trust the throwaway host key for [127.0.0.1]:PORT in the REAL known_hosts
# (restored on exit). Appends exactly one line.
add_known_host() { # add_known_host PORT
    local line
    line="[127.0.0.1]:$1 $(cat "$T/hostkey.pub")"
    (
        umask 077
        if [ -s "$KH" ] && [ "$(tail -c1 "$KH" | od -An -tx1 | tr -d ' \n')" != 0a ]; then
            printf '\n' >>"$KH"
        fi
        printf '%s\n' "$line" >>"$KH"
    ) || return 1
    printf '%s\n' "$line" >>"$KH_MINE"
}

make_keys() {
    ssh-keygen -q -t ed25519 -N '' -C moniker-e2e-host -f "$T/hostkey" &&
        ssh-keygen -q -t ed25519 -N '' -C moniker-e2e-client -f "$T/client_key" &&
        cp "$T/client_key.pub" "$T/authorized_keys"
}

# ---------- test helpers ----------

# Run a command with no stdin, saving stdout and stderr; sets RC.
capture() { "$@" </dev/null >"$OUT" 2>"$ERR"; RC=$?; }

err_summary() { tail -n 3 "$ERR" | tr '\n' '|' | cut -c1-300; }

# sha256 of one instance's profile file. The path comes from the product's own
# lookup (via the helper), not from a layout assumed here. Fails, printing
# nothing, if the path is outside that instance's config dir or the file cannot
# be hashed, so a missing file can never compare equal to another missing file.
profile_hash() { # profile_hash A|B
    local path h
    path=$("$1" "$HELPER" store-path 2>/dev/null) || return 1
    case $path in "$T/$1/cfg/"*) ;; *) return 1 ;; esac
    h=$(sha256sum -- "$path" 2>/dev/null | cut -d' ' -f1)
    [[ $h =~ ^[0-9a-f]{64}$ ]] || return 1
    printf '%s' "$h"
}

# The password of profile ID on instance A or B; a marker if there is none
# (get-secret exit 1); fails on any other error (unknown id, keyring trouble).
secret_of() {
    local v rc
    v=$("$1" "$HELPER" get-secret "$2" 2>/dev/null)
    rc=$?
    case $rc in
    0) printf '%s' "$v" ;;
    1) printf '\001none' ;;
    *) return 1 ;;
    esac
}

# One digest over every test password on both sides; shows no secret.
secrets_fingerprint() {
    local who id one all=""
    for who in A B; do
        for id in "$PA_ID" "$PB_ID"; do
            one=$(secret_of "$who" "$id") || return 1
            all+="$one"$'\n'
        done
    done
    printf '%s' "$all" | sha256sum | cut -d' ' -f1
}

# Everything a failed sync must leave alone, as "A-profiles B-profiles passwords".
state_snapshot() {
    local a b s
    a=$(profile_hash A) && b=$(profile_hash B) && s=$(secrets_fingerprint) || return 1
    printf '%s %s %s' "$a" "$b" "$s"
}

# Succeeds if the state is the one in $1 (from state_snapshot); otherwise sets
# REASON to say which part changed (or that it could not be read).
unchanged_since() {
    local after b_a b_b b_s a_a a_b a_s
    after=$(state_snapshot) || { REASON="cannot read the state after the sync"; return 1; }
    read -r b_a b_b b_s <<<"$1"
    read -r a_a a_b a_s <<<"$after"
    [ "$b_a" = "$a_a" ] || { REASON="A's profiles.json changed"; return 1; }
    [ "$b_b" = "$a_b" ] || { REASON="B's profiles.json changed"; return 1; }
    [ "$b_s" = "$a_s" ] || { REASON="a stored password changed"; return 1; }
}

set_sync_key() { printf '%s\n' "$2" | "$1" "${MONX[@]}" sync-key set --stdin >/dev/null; }

run_check() { # run_check NAME FUNCTION
    REASON=""
    if "$2"; then
        say "PASS: $1"
        PASSED=$((PASSED + 1))
    else
        say "FAIL: $1 -- ${REASON:-no reason recorded}"
        FAILED=$((FAILED + 1))
    fi
}

# ---------- fixtures ----------

seed_fixtures() {
    mkdir -p "$T/A/cfg" "$T/B/cfg" "$T/N/cfg" || return 1
    PW_PA="pw-$(random_token)"
    PW_PB="pw-$(random_token)"
    SYNC_KEY="sync-$(random_token)"
    OTHER_KEY="other-$(random_token)"
    PA_ID=$(A "$HELPER" seed-profile --name pa --host pa.example.test --user alice) || return 1
    PB_ID=$(B "$HELPER" seed-profile --name pb --host pb.example.test --user bob) || return 1
    printf '%s\n' "$PW_PA" | A "$HELPER" set-secret pa || return 1
    printf '%s\n' "$PW_PB" | B "$HELPER" set-secret pb || return 1
    A "$HELPER" seed-profile --name loopback --host 127.0.0.1 --port "$KEY_PORT" \
        --user "$ME" --key-file "$T/client_key" >/dev/null || return 1
    set_sync_key A "$SYNC_KEY" && set_sync_key B "$SYNC_KEY"
}

# ---------- checks ----------

check_serve_purity() {
    capture env XDG_CONFIG_HOME="$T/N/cfg" MONIKER_KEYRING_SERVICE=moniker-e2e-nokey "${MONX[@]}" sync-serve
    [ "$RC" -ne 0 ] || { REASON="exit status 0, expected a failure"; return 1; }
    [ "$(wc -c <"$OUT")" -eq 0 ] || { REASON="stdout has $(wc -c <"$OUT") bytes, expected 0"; return 1; }
    grep -qF "$MSG_NO_SYNC_KEY" "$ERR" || { REASON="unexpected message: $(err_summary)"; return 1; }
}

check_converge() {
    local names
    capture A "${MONX[@]}" sync loopback
    [ "$RC" -eq 0 ] || { REASON="moniker-server sync exited $RC: $(err_summary)"; return 1; }
    names=$(A "$HELPER" list)
    grep -qxF pb <<<"$names" || { REASON="A does not list pb"; return 1; }
    names=$(B "$HELPER" list)
    grep -qxF pa <<<"$names" || { REASON="B does not list pa"; return 1; }
    [ "$(secret_of A "$PB_ID")" = "$PW_PB" ] || { REASON="pb's password did not arrive on A"; return 1; }
    [ "$(secret_of B "$PA_ID")" = "$PW_PA" ] || { REASON="pa's password did not arrive on B"; return 1; }
}

check_wrong_key() {
    local before
    [ "$(secret_of A "$PA_ID")" = "$PW_PA" ] && [ "$(secret_of B "$PB_ID")" = "$PW_PB" ] ||
        { REASON="precondition: each instance's own password is not stored"; return 1; }
    before=$(state_snapshot) || { REASON="cannot read the state before the sync"; return 1; }
    set_sync_key B "$OTHER_KEY" || { REASON="could not change B's sync key"; return 1; }
    capture A "${MONX[@]}" sync loopback
    set_sync_key B "$SYNC_KEY" || { REASON="could not restore B's sync key"; return 1; }
    [ "$RC" -ne 0 ] || { REASON="exit status 0, expected a failure"; return 1; }
    grep -qF "$MSG_WRONG_KEY" "$ERR" || { REASON="unexpected message: $(err_summary)"; return 1; }
    unchanged_since "$before"
}

check_delete_propagates() {
    local names state rc
    A "$HELPER" delete-profile pa || { REASON="could not delete pa on A"; return 1; }
    capture A "${MONX[@]}" sync loopback
    [ "$RC" -eq 0 ] || { REASON="moniker-server sync exited $RC: $(err_summary)"; return 1; }
    names=$(B "$HELPER" list)
    ! grep -qxF pa <<<"$names" || { REASON="pa is still live on B"; return 1; }
    grep -qxF pb <<<"$names" || { REASON="pb disappeared from B"; return 1; }
    # The deletion must arrive as a tombstone (so it keeps propagating), and the
    # password must be gone for a profile B still knows about: exit 1, not an
    # error exit 2 from an unknown id or a keyring problem.
    state=$(B "$HELPER" profile-state "$PA_ID")
    [ "$state" = tombstone ] || { REASON="B's record of pa is '$state', expected a tombstone"; return 1; }
    B "$HELPER" get-secret "$PA_ID" >/dev/null 2>&1
    rc=$?
    [ "$rc" -eq 1 ] || { REASON="get-secret for the deleted pa on B exited $rc, expected 1 (no password)"; return 1; }
}

check_idempotent() {
    capture A "${MONX[@]}" sync loopback
    [ "$RC" -eq 0 ] || { REASON="moniker-server sync exited $RC: $(err_summary)"; return 1; }
    grep -qxF "added 0, updated 0, removed 0" "$OUT" || { REASON="got: $(tr '\n' ' ' <"$OUT")"; return 1; }
}

check_server_not_on_path() {
    local before
    [ "$(secret_of A "$PB_ID")" = "$PW_PB" ] && [ "$(secret_of B "$PB_ID")" = "$PW_PB" ] ||
        { REASON="precondition: pb's password is not stored on both sides"; return 1; }
    start_sshd nopath "$SYSTEM_PATH" key && add_known_host "$SSHD_PORT" || { REASON="cannot start the second sshd"; return 1; }
    A "$HELPER" seed-profile --name nopath --host 127.0.0.1 --port "$SSHD_PORT" \
        --user "$ME" --key-file "$T/client_key" >/dev/null || { REASON="cannot seed the nopath profile"; return 1; }
    before=$(state_snapshot) || { REASON="cannot read the state before the sync"; return 1; }
    capture A "${MONX[@]}" sync nopath
    [ "$RC" -ne 0 ] || { REASON="exit status 0, expected a failure"; return 1; }
    grep -qF "$MSG_NO_SERVER" "$ERR" || { REASON="unexpected message: $(err_summary)"; return 1; }
    unchanged_since "$before"
}

# ssh asks for the password of USER@HOST while our askpass wrapper logs the
# prompt text and the helper's exit status (never the answer).
write_askpass_wrapper() {
    cat >"$T/askpass-wrap.sh" <<EOF
#!/bin/sh
{ printf 'prompt=%s\n' "\$1"; printf 'kind=%s\n' "\${SSH_ASKPASS_PROMPT-}"; } >>"$T/askpass.log"
"$BIN/moniker-askpass" "\$1" >/dev/null
code=\$?
printf 'exit=%s\n' "\$code" >>"$T/askpass.log"
exit "\$code"
EOF
    chmod 700 "$T/askpass-wrap.sh"
}

# Seed a profile on A with a stored password; prints its id.
seed_password_profile() { # seed_password_profile NAME HOST
    local id
    id=$(A "$HELPER" seed-profile --name "$1" --host "$2" --port "$PW_PORT" --user "$ME") || return 1
    printf '%s\n' "pw-$(random_token)" | A "$HELPER" set-secret "$1" || return 1
    printf '%s' "$id"
}

# ssh to the password-only sshd with the wrapper as askpass and no controlling tty.
ssh_with_wrapper() { # ssh_with_wrapper PROFILE_ID TARGET_HOST
    : >"$T/askpass.log"
    capture A env SSH_ASKPASS="$T/askpass-wrap.sh" SSH_ASKPASS_REQUIRE=force MONIKER_PROFILE_ID="$1" \
        setsid -w timeout 30 ssh -F /dev/null -p "$PW_PORT" -o UserKnownHostsFile="$T/kh" \
        -o StrictHostKeyChecking=accept-new -o NumberOfPasswordPrompts=1 \
        -o PubkeyAuthentication=no -- "$ME@$2" true
}

expect_askpass_log() { # expect_askpass_log CASE PROMPT EXIT
    grep -qxF "prompt=$2" "$T/askpass.log" || { REASON="case $1: prompt not seen (log: $(tr '\n' '~' <"$T/askpass.log"))"; return 1; }
    grep -qxF "exit=$3" "$T/askpass.log" || { REASON="case $1: helper did not exit $3 (log: $(tr '\n' '~' <"$T/askpass.log"))"; return 1; }
}

check_askpass_prompts() {
    local id_a id_b id_c
    start_sshd pwauth "$SYSTEM_PATH" password || { REASON="cannot start the password sshd"; return 1; }
    PW_PORT=$SSHD_PORT
    write_askpass_wrapper
    id_a=$(seed_password_profile askpass-a 127.0.0.1) &&
        id_b=$(seed_password_profile askpass-b LOCALHOST) &&
        id_c=$(seed_password_profile askpass-c jump.example.test) || { REASON="cannot seed the profiles"; return 1; }
    ASKPASS_ID=$id_a
    ssh_with_wrapper "$id_a" 127.0.0.1
    expect_askpass_log a "$ME@127.0.0.1's password: " 0 || return 1
    # The profile's host is upper case, and so is the destination ssh gets:
    # OpenSSH itself lowercases the host in the prompt, which is what the
    # identity match (askpass::answer) expects.
    ssh_with_wrapper "$id_b" LOCALHOST
    expect_askpass_log b "$ME@localhost's password: " 0 || return 1
    ssh_with_wrapper "$id_c" 127.0.0.1
    expect_askpass_log c "$ME@127.0.0.1's password: " 1
}

check_host_key_not_auto_answered() {
    [ -n "$ASKPASS_ID" ] || { REASON="no stored-password profile (the previous check did not set it up)"; return 1; }
    : >"$T/kh2"
    capture A env SSH_ASKPASS="$BIN/moniker-askpass" SSH_ASKPASS_REQUIRE=force MONIKER_PROFILE_ID="$ASKPASS_ID" \
        setsid -w timeout 30 ssh -F /dev/null -p "$KEY_PORT" -o UserKnownHostsFile="$T/kh2" \
        -o StrictHostKeyChecking=ask -o NumberOfPasswordPrompts=1 -- "$ME@127.0.0.1" true
    [ "$RC" -ne 0 ] || { REASON="ssh connected"; return 1; }
    grep -qF "Host key verification failed" "$ERR" || { REASON="unexpected ssh output: $(err_summary)"; return 1; }
    [ ! -s "$T/kh2" ] || { REASON="the host key was written to known_hosts"; return 1; }
}

# ---------- inner main ----------

inner_main() {
    T=${E2E_T:-} KH=${E2E_KH:-} BIN=${E2E_BIN:-}
    assert_private_env
    set_kh_paths
    trap inner_cleanup EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM HUP
    MON=$BIN/moniker-server HELPER=$BIN/examples/e2e_helper
    # Every moniker-server run gets its own session (no controlling terminal, so a
    # prompt on /dev/tty fails at once instead of waiting) and a time limit.
    MONX=(setsid -w timeout 60 "$MON")
    OUT=$T/out ERR=$T/err

    say "== private Secret Service"
    start_keyring || { warn "e2e-sync: private Secret Service unavailable (see $T/gkd.log)"; exit 3; }
    make_keys || { warn "e2e-sync: cannot generate keys"; exit 3; }
    start_sshd main "$BIN:$SYSTEM_PATH" key || exit 3
    KEY_PORT=$SSHD_PORT
    add_known_host "$KEY_PORT" || exit 3
    seed_fixtures || { warn "e2e-sync: cannot seed the two instances"; exit 3; }

    say "== checks"
    run_check "sync-serve stdout purity" check_serve_purity
    run_check "converge" check_converge
    run_check "wrong sync key" check_wrong_key
    run_check "delete propagates" check_delete_propagates
    run_check "idempotent" check_idempotent
    run_check "moniker-server not on remote PATH" check_server_not_on_path
    run_check "askpass prompt format with real OpenSSH" check_askpass_prompts
    run_check "host key confirmation is never auto-answered" check_host_key_not_auto_answered

    say "== summary: $PASSED passed, $FAILED failed"
    [ "$FAILED" -eq 0 ]
}

if [ "${MONIKER_E2E_INNER:-}" = 1 ]; then
    inner_main
    exit $?
fi
outer_main
