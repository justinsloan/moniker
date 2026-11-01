#!/usr/bin/env bash
# End-to-end check that two Moniker instances sync profiles and passwords over
# SSH. Run it from anywhere; it builds what it needs first.
#
# How it works
#   The script re-runs itself ("inner" phase) under `env -i` inside its own
#   private D-Bus session, with a private gnome-keyring-daemon (Secret Service)
#   that it creates and unlocks itself, and throwaway user-level sshd servers
#   on 127.0.0.1 (key login; key login without `moniker-server` on PATH; password
#   login; password and key login for the jump connection checks). Instance A is
#   the script's own side, instance B is whatever `moniker-server sync-serve`
#   runs inside the sshd session. Passwords and sync keys
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
PW_PORT=
KEYRING_PW=moniker-e2e-throwaway # protects only the throwaway keyring in $T
# Display texts of moniker-core's SyncError::WrongSyncKey and ::ServerNotFound
# (crates/moniker-core/src/sync.rs), and the CLI's own "no sync key" message
# (crates/moniker-cli/src/main.rs, sync_serve); the CLI prints them on stderr.
MSG_NO_SYNC_KEY='no sync key is set on the remote machine (where `moniker-server sync-serve` runs); run `moniker-server sync-key set` there'
MSG_WRONG_KEY="the sync passphrases differ (or the data was corrupted on the way)"
MSG_NO_SERVER="moniker-server is not installed on the remote machine (or not on its PATH)"
# SyncError::PeerTooOld: a machine older than jump hosts turned a sync away.
MSG_PEER_TOO_OLD="the other machine runs an older Moniker, which does not know jump hosts; update it to sync while a profile has a jump host"

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
write_sshd_config() { # write_sshd_config FILE PORT NAME REMOTE_PATH key|password|both
    local auth="PasswordAuthentication no" pubkey="PubkeyAuthentication yes" penalties="" listen6=""
    # A user-level sshd cannot check passwords, so every password login fails;
    # OpenSSH 9.8 and later then penalize the source address, 127.0.0.1, and
    # drop the next checks' connections. The password servers have none. "both"
    # offers the key as well, so that a login which tried the password first
    # (the askpass helper answered it) can still succeed with the key; it also
    # listens on ::1, for a target that is an IPv6 address.
    [ "$5" = password ] && { auth="PasswordAuthentication yes"; pubkey="PubkeyAuthentication no"; penalties="PerSourcePenalties no"; }
    [ "$5" = both ] && { auth="PasswordAuthentication yes"; penalties="PerSourcePenalties no"; listen6="ListenAddress ::1"; }
    cat >"$1" <<EOF
Port $2
ListenAddress 127.0.0.1
$listen6
HostKey $T/hostkey
AuthorizedKeysFile $T/authorized_keys
PidFile $T/sshd_$3.pid
$auth
$pubkey
$penalties
KbdInteractiveAuthentication no
UsePAM no
PermitUserRC no
StrictModes no
LogLevel VERBOSE
SetEnv XDG_CONFIG_HOME=$T/B/cfg MONIKER_KEYRING_SERVICE=moniker-e2e-b DBUS_SESSION_BUS_ADDRESS=unix:path=$T/run/bus XDG_RUNTIME_DIR=$T/run XDG_DATA_HOME=$T/data HOME=$T/home PATH=$4
EOF
}

port_speaks_ssh() { # port_speaks_ssh PORT [ADDRESS]: it answers with an SSH banner (on 127.0.0.1)
    local banner
    banner=$( (exec 3<>"/dev/tcp/${2:-127.0.0.1}/$1" && read -r -t 2 line <&3 && printf '%s' "$line") 2>/dev/null)
    case $banner in SSH-2.0-*) return 0 ;; *) return 1 ;; esac
}

# Start a throwaway sshd; sets SSHD_PORT. Retries on another port if it fails.
start_sshd() { # start_sshd NAME REMOTE_PATH key|password|both
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
# prompt text and the helper's exit status (never the answer). The helper
# answers only for an ssh whose parent is MONIKER_PID; with the wrapper between
# ssh and the helper, the helper's grandparent is ssh itself, so the wrapper
# names that (its own parent). The checks with askpass-exec.sh below keep the
# real process tree instead.
write_askpass_wrapper() {
    cat >"$T/askpass-wrap.sh" <<EOF
#!/bin/sh
{ printf 'prompt=%s\n' "\$1"; printf 'kind=%s\n' "\${SSH_ASKPASS_PROMPT-}"; } >>"$T/askpass.log"
MONIKER_PID=\$PPID "$BIN/moniker-askpass" "\$1" >/dev/null
code=\$?
printf 'exit=%s\n' "\$code" >>"$T/askpass.log"
exit "\$code"
EOF
    chmod 700 "$T/askpass-wrap.sh"
    # Logs the prompt, then becomes the helper: ssh -> helper, as with Moniker.
    cat >"$T/askpass-exec.sh" <<EOF
#!/bin/sh
printf 'prompt=%s\n' "\$1" >>"$T/askpass.log"
exec "$BIN/moniker-askpass" "\$1"
EOF
    chmod 700 "$T/askpass-exec.sh"
}

# An `ssh` first on PATH that reads $T/ssh_config instead of ~/.ssh/config:
# every ssh of the askpass checks goes through it, the helper's own `ssh -G`
# and a ProxyJump hop's ssh included, so they all see the same configuration
# and the real one is never read. `web-alias` is rewritten by HostName;
# `hopped` is reached through a ProxyJump hop whose prompt names the same
# user@host as the target's: the target's prompt names its HostKeyAlias,
# 127.0.0.1, the hop's its host, also 127.0.0.1 (OpenSSH refuses a jump to the
# very host, port and user it connects to, hence the alias). $VIA_JUMP_HOST is
# the jump host of the checks of connections through one, and
# $VIA_TARGET_HOST, $VIA_CANON_HOST and ::1 their targets: each signs in with
# the key after the password (see "jump connections" below). The first
# target's entry has a ProxyJump of its own, to a host that does not exist, and
# names its host key 127.0.0.1 while its host name is localhost; the second's
# has no route and canonicalizes its host name, localhost, to
# localhost.localhost (which ssh does only for a connection without a proxy).
# $SYNC_PARTNER_HOST is the partner of the sync through a jump host, whose
# entry also has a ProxyJump to a host that does not exist, so that it can only
# be reached by the route on the command line; $OLD_HOST is the machine that
# predates jump hosts. Their host keys and the jump host's are in $T/kh before
# the sync, which pins StrictHostKeyChecking=yes (trust_in_shim_kh).
write_ssh_shim() {
    local real
    real=$(command -v ssh) || return 1
    mkdir -p "$T/shim" || return 1
    cat >"$T/shim/ssh" <<EOF
#!/bin/sh
exec "$real" -F "$T/ssh_config" "\$@"
EOF
    chmod 700 "$T/shim/ssh"
    cat >"$T/ssh_config" <<EOF
Host web-alias
  HostName 127.0.0.1
Host hopped
  HostName localhost
  HostKeyAlias 127.0.0.1
  ProxyJump $ME@127.0.0.1:$PW_PORT
Host $VIA_JUMP_HOST
  HostName 127.0.0.1
Host $VIA_TARGET_HOST
  HostName localhost
  HostKeyAlias 127.0.0.1
  ProxyJump nowhere.invalid
Host $VIA_CANON_HOST
  HostName localhost
  CanonicalizeHostname yes
  CanonicalDomains localhost
Host $SYNC_PARTNER_HOST
  HostName 127.0.0.1
  ProxyJump nowhere.invalid
Host $OLD_HOST
  HostName 127.0.0.1
Host $VIA_JUMP_HOST $VIA_TARGET_HOST $VIA_CANON_HOST $SYNC_PARTNER_HOST $OLD_HOST ::1
  PubkeyAuthentication yes
  IdentitiesOnly yes
  IdentityFile $T/client_key
  PreferredAuthentications password,publickey
Host *
  UserKnownHostsFile $T/kh
  StrictHostKeyChecking accept-new
  NumberOfPasswordPrompts 1
  PubkeyAuthentication no
EOF
}

# Seed a profile on A with a stored password, going through the profile named
# JUMP_HOST if one is given; prints its id.
seed_password_profile() { # seed_password_profile NAME HOST [PORT [JUMP_HOST]]
    local id via=()
    [ -z "${4:-}" ] || via=(--jump-host "$4")
    id=$(A "$HELPER" seed-profile --name "$1" --host "$2" --port "${3:-$PW_PORT}" --user "$ME" "${via[@]}") || return 1
    printf '%s\n' "pw-$(random_token)" | A "$HELPER" set-secret "$1" || return 1
    printf '%s' "$id"
}

# ssh to the password-only sshd with the wrapper as askpass and no controlling tty.
ssh_with_wrapper() { # ssh_with_wrapper PROFILE_ID TARGET_HOST
    : >"$T/askpass.log"
    capture A env PATH="$T/shim:$PATH" SSH_ASKPASS="$T/askpass-wrap.sh" SSH_ASKPASS_REQUIRE=force \
        MONIKER_PROFILE_ID="$1" setsid -w timeout 30 ssh -p "$PW_PORT" -- "$ME@$2" true
}

# The same with askpass-exec.sh and a one-time token, started the way Moniker
# starts it: by a process (here a shell) whose id is MONIKER_PID, ssh being its
# child. Whether the helper answered from the keyring shows in the token's
# marker, which only an answering helper creates.
ssh_like_moniker() { # ssh_like_moniker PROFILE_ID TARGET_HOST TOKEN
    : >"$T/askpass.log"
    # `exit $?` after ssh keeps the shell from exec'ing it: ssh stays its child.
    # shellcheck disable=SC2016
    capture A env PATH="$T/shim:$PATH" SSH_ASKPASS="$T/askpass-exec.sh" SSH_ASKPASS_REQUIRE=force \
        MONIKER_PROFILE_ID="$1" MONIKER_ASKPASS_TOKEN="$3" setsid -w timeout 30 \
        sh -c 'MONIKER_PID=$$; export MONIKER_PID; ssh -p "$1" -- "$2" true; exit $?' sh "$PW_PORT" "$ME@$2"
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
    write_ssh_shim || { REASON="cannot write the ssh shim"; return 1; }
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

# The helper follows ~/.ssh/config (here $T/ssh_config) through `ssh -G`, and
# never answers a ProxyJump hop's prompt, even one that names exactly the
# target's user@host.
check_askpass_follows_ssh_config() {
    local id_d id_e token marker
    [ -n "$PW_PORT" ] || { REASON="no password sshd (the previous check did not set it up)"; return 1; }
    id_d=$(seed_password_profile askpass-d web-alias) &&
        id_e=$(seed_password_profile askpass-e hopped) || { REASON="cannot seed the profiles"; return 1; }
    # d: the profile's host is an alias that HostName rewrites; ssh's prompt
    # names the real host, and the helper answers it from the keyring.
    token=$(cat /proc/sys/kernel/random/uuid)
    marker=$T/run/moniker/askpass/$token
    ssh_like_moniker "$id_d" web-alias "$token"
    grep -qxF "prompt=$ME@127.0.0.1's password: " "$T/askpass.log" || { REASON="case d: prompt not seen (log: $(tr '\n' '~' <"$T/askpass.log"); ssh: $(err_summary))"; return 1; }
    [ -e "$marker" ] || { REASON="case d: the stored password was not sent for the alias"; return 1; }
    rm -f -- "$marker"
    # e: the hop's ssh prompts first, naming the same user@host as the target;
    # it is not the ssh that was started, so it gets nothing.
    token=$(cat /proc/sys/kernel/random/uuid)
    marker=$T/run/moniker/askpass/$token
    ssh_like_moniker "$id_e" hopped "$token"
    grep -qxF "prompt=$ME@127.0.0.1's password: " "$T/askpass.log" || { REASON="case e: the hop's prompt not seen (log: $(tr '\n' '~' <"$T/askpass.log"); ssh: $(err_summary))"; return 1; }
    [ ! -e "$marker" ] || { REASON="case e: the hop's prompt got the stored password"; return 1; }
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

# ---------- jump connections through a shared ssh connection (issue #9) ----------
#
# The jump host is opened as the master of a shared ssh connection (-f -N -M -S
# SOCKET), and the target's ssh reaches the target through it with a
# ProxyCommand (-W). The checks show how OpenSSH behaves for exactly that: the
# master's exit status, parent and pid, the socket, the hop, how the master
# ends, and that no TCP port is opened. Every ssh is started the way Moniker
# starts one (see ssh_like_moniker): a shell whose id is MONIKER_PID, the
# askpass helper, a one-time token and the profile's id.
#
# Both sshds take a password and a key. A user-level sshd cannot check a
# password, so a password-only server never lets a login succeed, and the
# master could not sign in. ssh offers the password first: the helper answers
# it (the token's marker shows that) and the sshd refuses it; then the key
# signs in.

JUMP_PORT=    # sshd of the jump host
TARGET_PORT=  # sshd of the target
JUMP_ID=      # profile ids, each with a stored password: the jump host, the target
TARGET_ID=
JUMP_DIR=     # where the sockets are
JUMP_READY=   # set once jump_setup has done everything
JUMP_SIGNIN=()  # ssh options: the password first, then the key
JUMP_SOCKETS=() # sockets of the masters a check started
JUMP_MASTERS=() # ids of those masters, once known (learn_master)
MASTER_PID=     # set by learn_master
VIA_JUMP_HOST=bastion-e2e # hosts of the profiles of the connections through a jump host
VIA_TARGET_HOST=db-e2e    # (their entries in $T/ssh_config: write_ssh_shim)
VIA_CANON_HOST=canon-e2e
SYNC_PARTNER_HOST=partner-e2e # hosts of the syncs through a jump host (see write_ssh_shim)
OLD_HOST=old-e2e
VIA_READY=      # set once via_setup has seeded them
VIA_JUMP_ID=    # their ids
VIA_DB_ID=
VIA_CANON_ID=
VIA_V6_ID=
CMD_ARGS=()     # a command the product built (built_command): the program, then its arguments
CMD_ENV=()      # and the variables it adds, NAME=value

new_token() { cat /proc/sys/kernel/random/uuid; }
jump_socket() { printf '%s/%s' "$JUMP_DIR" "$(random_token | cut -c1-32)"; }
now_ms() { local t=${EPOCHREALTIME/[.,]/}; printf '%s' $((t / 1000)); }
marker_of() { printf '%s' "$T/run/moniker/askpass/$1"; } # the file the helper creates when it answers
prompts_asked() { grep -c '^prompt=' "$T/askpass.log"; }

# The process PID with start time START is running (a zombie is not).
proc_alive() { # proc_alive PID START
    local stat
    stat=$(cat "/proc/$1/stat" 2>/dev/null) || return 1
    stat=${stat##*) }
    [ "${stat%% *}" != Z ] && [ "$(proc_start "$1")" = "$2" ]
}

# The parent of a process (field 4 of /proc/PID/stat).
proc_ppid() {
    local stat
    stat=$(cat "/proc/$1/stat" 2>/dev/null) || return 1
    stat=${stat##*) }
    # shellcheck disable=SC2086
    set -- $stat
    printf '%s' "$2"
}

# Wait up to LIMIT_MS for the process to be gone and the socket removed. Prints
# "P S", the milliseconds each took ('-' = still there at the limit); succeeds
# if both went.
wait_gone() { # wait_gone PID START SOCK LIMIT_MS
    local t0 t p=- s=-
    t0=$(now_ms)
    while :; do
        t=$(($(now_ms) - t0))
        [ "$p" = - ] && ! proc_alive "$1" "$2" && p=$t
        [ "$s" = - ] && [ ! -e "$3" ] && s=$t
        [ "$p" != - ] && [ "$s" != - ] && break
        [ "$t" -ge "$4" ] && break
        sleep 0.05
    done
    printf '%s %s' "$p" "$s"
    [ "$p" != - ] && [ "$s" != - ]
}

# Run ssh ARG... as Moniker runs a connection (see ssh_like_moniker) for the
# profile and the one-time token given. The launching shell's id goes to
# $T/launcher.pid.
ssh_args_like_moniker() { # ssh_args_like_moniker PROFILE_ID TOKEN ARG...
    local id=$1 token=$2
    shift 2
    : >"$T/askpass.log"
    # shellcheck disable=SC2016
    capture A env PATH="$T/shim:$PATH" SSH_ASKPASS="$T/askpass-exec.sh" SSH_ASKPASS_REQUIRE=force \
        MONIKER_PROFILE_ID="$id" MONIKER_ASKPASS_TOKEN="$token" setsid -w timeout 30 \
        sh -c 'f=$1; shift; MONIKER_PID=$$; export MONIKER_PID; echo "$$" >"$f"; ssh "$@"; exit $?' \
        sh "$T/launcher.pid" "$@"
}

# The two sshds, the two profiles and the socket directory, once.
jump_setup() {
    [ -z "$JUMP_READY" ] || return 0
    [ -x "$T/askpass-exec.sh" ] && [ -x "$T/shim/ssh" ] ||
        { REASON="no askpass script or ssh shim (the askpass checks did not set them up)"; return 1; }
    start_sshd pwjump "$SYSTEM_PATH" both || { REASON="cannot start the jump host's sshd"; return 1; }
    JUMP_PORT=$SSHD_PORT
    start_sshd pwtarget "$SYSTEM_PATH" both || { REASON="cannot start the target's sshd"; return 1; }
    TARGET_PORT=$SSHD_PORT
    JUMP_ID=$(seed_password_profile jump-host 127.0.0.1 "$JUMP_PORT") || { REASON="cannot seed the jump host's profile"; return 1; }
    TARGET_ID=$(seed_password_profile jump-target 127.0.0.1 "$TARGET_PORT") || { REASON="cannot seed the target's profile"; return 1; }
    JUMP_DIR=$T/run/moniker/jump
    mkdir -p "$T/run/moniker" "$JUMP_DIR" && chmod 700 "$T/run/moniker" "$JUMP_DIR" ||
        { REASON="cannot make the socket directory"; return 1; }
    JUMP_SIGNIN=(-o PubkeyAuthentication=yes -o IdentitiesOnly=yes -i "$T/client_key"
        -o PreferredAuthentications=password,publickey)
    JUMP_READY=1
}

# The jump connection: ssh to the jump host, in the background after it signed
# in, as the master of a shared connection at SOCK. Succeeds if the master
# exited 0 and the helper answered its prompt.
start_master() { # start_master SOCK TOKEN
    ssh_args_like_moniker "$JUMP_ID" "$2" -p "$JUMP_PORT" "${JUMP_SIGNIN[@]}" \
        -f -N -M -S "$1" -o ControlPersist=10 -- "$ME@127.0.0.1"
    [ "$RC" -eq 0 ] || { REASON="the master exited $RC, expected 0: $(err_summary)"; return 1; }
    JUMP_SOCKETS+=("$1")
    [ -e "$(marker_of "$2")" ] ||
        { REASON="the helper did not answer the master's prompt (askpass log: $(tr '\n' '~' <"$T/askpass.log"))"; return 1; }
}

# Ask the master at SOCK for its id the way Moniker will (-O check), and record
# the process so that it is stopped in any case. Sets MASTER_PID.
learn_master() { # learn_master SOCK
    capture env PATH="$T/shim:$PATH" ssh -S "$1" -O check -- "$ME@127.0.0.1"
    [ "$RC" -eq 0 ] || { REASON="-O check exited $RC: $(err_summary)"; return 1; }
    MASTER_PID=$(tr -d '\r' <"$ERR" | sed -n 's/^Master running (pid=\([0-9][0-9]*\))$/\1/p')
    [ -n "$MASTER_PID" ] || { REASON="-O check said: $(err_summary)"; return 1; }
    register_pid "$MASTER_PID"
    JUMP_MASTERS+=("$MASTER_PID")
}

# The target's ssh, reaching the target through the master at SOCK with a
# ProxyCommand. HOP_OPTION... go to the hop's ssh (BatchMode=yes by default).
run_target() { # run_target SOCK TOKEN COMMAND [HOP_OPTION...]
    local sock=$1 token=$2 cmd=$3 hop="-o BatchMode=yes"
    shift 3
    [ "$#" -eq 0 ] || hop=$*
    ssh_args_like_moniker "$TARGET_ID" "$token" -p "$TARGET_PORT" "${JUMP_SIGNIN[@]}" \
        -o "ProxyCommand=ssh -S $sock -o ControlMaster=no $hop -W %h:%p -p $JUMP_PORT -- $ME@127.0.0.1" \
        -- "$ME@127.0.0.1" "$cmd"
}

# End the master at SOCK with -O exit.
end_master() { # end_master SOCK
    capture env PATH="$T/shim:$PATH" ssh -S "$1" -O exit -- "$ME@127.0.0.1"
    [ "$RC" -eq 0 ] || { REASON="-O exit exited $RC: $(err_summary)"; return 1; }
}

# Stop every master a check left. They are not our children: -O exit through
# the socket this script made, and then the recorded id and start time (stop_pid)
# for one that did not answer. Never a process found by name.
jump_stop_all() {
    local sock pid
    for sock in "${JUMP_SOCKETS[@]}"; do
        [ -S "$sock" ] && timeout 5 env PATH="$T/shim:$PATH" ssh -S "$sock" -O exit -- "$ME@127.0.0.1" </dev/null >/dev/null 2>&1
    done
    for pid in "${JUMP_MASTERS[@]}"; do stop_pid "$pid"; done
    JUMP_SOCKETS=() JUMP_MASTERS=()
}

run_jump_check() { # run_jump_check NAME FUNCTION: run_check, then end any master the check left
    run_check "$1" "$2"
    jump_stop_all
}

check_jump_master_signs_in() {
    local sock token parent launcher
    jump_setup || return 1
    sock=$(jump_socket)
    token=$(new_token)
    start_master "$sock" "$token" || return 1
    [ -S "$sock" ] || { REASON="no socket at $sock"; return 1; }
    learn_master "$sock" || return 1
    grep -qx "Master running (pid=$MASTER_PID)" <(tr -d '\r' <"$ERR") ||
        { REASON="-O check did not say 'Master running (pid=$MASTER_PID)': $(err_summary)"; return 1; }
    parent=$(proc_ppid "$MASTER_PID") launcher=$(cat "$T/launcher.pid")
    [ "$parent" != "$launcher" ] || { REASON="the master's parent is still the launching shell ($launcher)"; return 1; }
    end_master "$sock"
}

check_target_through_the_socket() {
    local sock token
    jump_setup || return 1
    sock=$(jump_socket)
    start_master "$sock" "$(new_token)" && learn_master "$sock" || return 1
    token=$(new_token)
    run_target "$sock" "$token" true
    [ "$RC" -eq 0 ] || { REASON="the target command exited $RC, expected 0: $(err_summary)"; return 1; }
    [ -e "$(marker_of "$token")" ] || { REASON="the helper did not answer the target's prompt"; return 1; }
    [ "$(prompts_asked)" -eq 1 ] ||
        { REASON="expected the target's prompt only, got $(prompts_asked) prompts (log: $(tr '\n' '~' <"$T/askpass.log"))"; return 1; }
    end_master "$sock"
}

check_hop_without_master() {
    local sock token
    jump_setup || return 1
    sock=$(jump_socket)
    start_master "$sock" "$(new_token)" && learn_master "$sock" || return 1
    end_master "$sock" || return 1
    wait_gone "$MASTER_PID" "${PID_START[$MASTER_PID]}" "$sock" 3000 >/dev/null ||
        { REASON="the master did not end after -O exit"; return 1; }
    # The hop in BatchMode: it finds no master, would have to sign in itself,
    # and may not ask.
    token=$(new_token)
    run_target "$sock" "$token" true
    [ "$RC" -ne 0 ] || { REASON="the target command exited 0 without a master"; return 1; }
    [ ! -e "$(marker_of "$token")" ] || { REASON="the stored password was sent without a master"; return 1; }
    [ "$(prompts_asked)" -eq 0 ] || { REASON="a prompt was asked without a master (log: $(tr '\n' '~' <"$T/askpass.log"))"; return 1; }
    # The hop that may ask: its prompt names the target's user@host, and it is
    # not the ssh Moniker started, so the helper gives it nothing.
    token=$(new_token)
    run_target "$sock" "$token" true -o BatchMode=no
    [ "$RC" -ne 0 ] || { REASON="the target command exited 0 without a master (hop may ask)"; return 1; }
    [ "$(prompts_asked)" -ge 1 ] || { REASON="the hop that may ask did not ask, so nothing was tested"; return 1; }
    [ ! -e "$(marker_of "$token")" ] || { REASON="the stored password was sent to the hop"; return 1; }
}

check_master_ends_at_once() {
    local sock pid start took
    jump_setup || return 1
    # -O exit
    sock=$(jump_socket)
    start_master "$sock" "$(new_token)" && learn_master "$sock" || return 1
    pid=$MASTER_PID start=${PID_START[$MASTER_PID]}
    end_master "$sock" || return 1
    took=$(wait_gone "$pid" "$start" "$sock" 1000) || { REASON="after -O exit (process ms, socket ms): $took"; return 1; }
    # SIGTERM to the id this script recorded
    sock=$(jump_socket)
    start_master "$sock" "$(new_token)" && learn_master "$sock" || return 1
    pid=$MASTER_PID start=${PID_START[$MASTER_PID]}
    proc_alive "$pid" "$start" || { REASON="the master is not running before SIGTERM"; return 1; }
    kill -TERM "$pid"
    took=$(wait_gone "$pid" "$start" "$sock" 1000) || { REASON="after SIGTERM (process ms, socket ms): $took"; return 1; }
}

check_master_backstop() {
    local sock pid start took p
    jump_setup || return 1
    # ControlPersist=10 ends a master 9.3 to 9.8 s after its last client closed
    # (measured; the clock evidently counts whole seconds), so 8 s is the least
    # it may take, allowing for the time the client needed to exit.
    # No client at all: the clock restarts when -O check closes.
    sock=$(jump_socket)
    start_master "$sock" "$(new_token)" && learn_master "$sock" || return 1
    pid=$MASTER_PID start=${PID_START[$MASTER_PID]}
    took=$(wait_gone "$pid" "$start" "$sock" 13000) || { REASON="a master with no client after 13 s (process ms, socket ms): $took"; return 1; }
    p=${took%% *}
    [ "$p" -ge 8000 ] || { REASON="a master with no client ended after only $p ms"; return 1; }
    # One session, which ends.
    sock=$(jump_socket)
    start_master "$sock" "$(new_token)" && learn_master "$sock" || return 1
    pid=$MASTER_PID start=${PID_START[$MASTER_PID]}
    run_target "$sock" "$(new_token)" true
    [ "$RC" -eq 0 ] || { REASON="the target command exited $RC: $(err_summary)"; return 1; }
    took=$(wait_gone "$pid" "$start" "$sock" 13000) || { REASON="a master whose session ended, after 13 s (process ms, socket ms): $took"; return 1; }
    p=${took%% *}
    [ "$p" -ge 8000 ] || { REASON="a master whose session ended went after only $p ms"; return 1; }
}

check_no_listening_port() {
    local sock new
    jump_setup || return 1
    ss -lntH | awk '{print $4}' | sort -u >"$T/ss.before"
    sock=$(jump_socket)
    start_master "$sock" "$(new_token)" && learn_master "$sock" || return 1
    # The command runs on the target while the session is open.
    run_target "$sock" "$(new_token)" "$(command -v ss) -lntH >$T/ss.during"
    [ "$RC" -eq 0 ] || { REASON="the target command exited $RC: $(err_summary)"; return 1; }
    [ -s "$T/ss.during" ] || { REASON="the listing taken during the session is empty"; return 1; }
    awk '{print $4}' "$T/ss.during" | sort -u >"$T/ss.during.sorted"
    new=$(comm -13 "$T/ss.before" "$T/ss.during.sorted" | tr '\n' ' ')
    [ -z "$new" ] || { REASON="new listening sockets: $new"; return 1; }
    end_master "$sock"
}

# The command STAGE (master, target, check or exit) from what
# `e2e_helper connection-argv` printed to FILE, into CMD_ARGS and CMD_ENV.
built_command() { # built_command FILE STAGE
    local line stage=
    CMD_ARGS=() CMD_ENV=()
    while IFS= read -r line; do
        case $line in
        "arg "*) [ "$stage" != "$2" ] || CMD_ARGS+=("${line#arg }") ;;
        "env "*) [ "$stage" != "$2" ] || CMD_ENV+=("${line#env }") ;;
        *) stage=$line ;;
        esac
    done <"$1"
    [ "${#CMD_ARGS[@]}" -gt 0 ] || { REASON="connection-argv printed no $2 command"; return 1; }
}

# CMD_ENV is what Moniker gives an ssh for the profile PROFILE_ID with a stored
# password and the one-time token TOKEN: the five variables, in order, the
# askpass helper being $T/askpass-exec.sh. MONIKER_PID is the id of the helper
# that printed it; run_built puts the launching process's own id in its place.
expect_built_env() { # expect_built_env STAGE PROFILE_ID TOKEN
    local got want
    got=$(printf '%s~' "${CMD_ENV[@]}" | sed 's/MONIKER_PID=[0-9][0-9]*~/MONIKER_PID=N~/')
    want="SSH_ASKPASS=$T/askpass-exec.sh~SSH_ASKPASS_REQUIRE=force~MONIKER_PROFILE_ID=$2~MONIKER_PID=N~MONIKER_ASKPASS_TOKEN=$3~"
    [ "$got" = "$want" ] || { REASON="the $1's variables are $got, expected $want"; return 1; }
}

# Run CMD_ARGS with CMD_ENV as Moniker runs a command it built: started by a
# process (a shell) whose id is MONIKER_PID, ssh its child, through the shim
# (first on PATH, as `ssh`), with no input.
run_built() {
    : >"$T/askpass.log"
    # shellcheck disable=SC2016
    capture A env PATH="$T/shim:$PATH" "${CMD_ENV[@]}" setsid -w timeout 30 \
        sh -c 'MONIKER_PID=$$; export MONIKER_PID; "$@"; exit $?' sh "${CMD_ARGS[@]}"
}

# Run a control command the product built (CMD_ARGS, no variables of its own).
run_built_control() {
    capture env PATH="$T/shim:$PATH" timeout 5 "${CMD_ARGS[@]}"
}

# The profiles of the connections through a jump host, once: the jump host
# and three targets through it, each with a stored password and an entry in
# $T/ssh_config (write_ssh_shim) that signs in with the key once the password
# was answered and refused, so nothing is added to the command line.
via_setup() {
    [ -z "$VIA_READY" ] || return 0
    jump_setup || return 1
    VIA_JUMP_ID=$(seed_password_profile via-bastion "$VIA_JUMP_HOST" "$JUMP_PORT") &&
        VIA_DB_ID=$(seed_password_profile via-db "$VIA_TARGET_HOST" "$TARGET_PORT" via-bastion) &&
        VIA_CANON_ID=$(seed_password_profile via-canon "$VIA_CANON_HOST" "$TARGET_PORT" via-bastion) &&
        VIA_V6_ID=$(seed_password_profile via-v6 ::1 "$TARGET_PORT" via-bastion) ||
        { REASON="cannot seed the profiles"; return 1; }
    VIA_READY=1
}

# Connect to the profile NAME (id ID) through its jump host with exactly the
# commands the product builds (e2e_helper connection-argv: a socket made by
# JumpSocket::new in $XDG_RUNTIME_DIR, then jump::pane_commands), run as
# Moniker runs them: the master signs in, its prompt answered by the helper;
# -O check names it; the target's ssh goes through it, its own prompt (naming
# PROMPT_HOST) answered, none from the hop, and the target's sshd lets it in;
# -O exit ends the master at once.
connect_through_jump_host() { # connect_through_jump_host NAME ID PROMPT_HOST
    local out=$T/connection-argv sock jump_token token accepted took
    jump_token=$(new_token) token=$(new_token)
    A "$HELPER" connection-argv --profile "$1" --askpass "$T/askpass-exec.sh" \
        --token "$token" --jump-token "$jump_token" >"$out" 2>"$ERR" ||
        { REASON="connection-argv failed: $(err_summary)"; return 1; }
    sock=$(sed -n 's/^socket //p' "$out")
    case $sock in "$JUMP_DIR"/*) ;; *) REASON="the socket is not in $JUMP_DIR: $sock"; return 1 ;; esac
    [ ! -e "$sock" ] || { REASON="something is already at the socket's path $sock"; return 1; }

    # The master signs in to the jump host: its prompt answered by the helper.
    built_command "$out" master && expect_built_env master "$VIA_JUMP_ID" "$jump_token" || return 1
    JUMP_SOCKETS+=("$sock")
    run_built
    [ "$RC" -eq 0 ] || { REASON="the master exited $RC, expected 0: $(err_summary)"; return 1; }
    [ -e "$(marker_of "$jump_token")" ] ||
        { REASON="the helper did not answer the master's prompt (askpass log: $(tr '\n' '~' <"$T/askpass.log"))"; return 1; }
    [ "$(prompts_asked)" -eq 1 ] ||
        { REASON="the master asked $(prompts_asked) prompts, expected 1 (log: $(tr '\n' '~' <"$T/askpass.log"))"; return 1; }

    # -O check names the master, which is recorded so that it is stopped anyway.
    built_command "$out" check || return 1
    run_built_control
    [ "$RC" -eq 0 ] || { REASON="-O check exited $RC: $(err_summary)"; return 1; }
    MASTER_PID=$(tr -d '\r' <"$ERR" | sed -n 's/^Master running (pid=\([0-9][0-9]*\))$/\1/p')
    [ -n "$MASTER_PID" ] || { REASON="-O check said: $(err_summary)"; return 1; }
    register_pid "$MASTER_PID"
    JUMP_MASTERS+=("$MASTER_PID")

    # The target, through the master: its own prompt answered, none from the
    # hop, and the target's sshd let it in.
    built_command "$out" target && expect_built_env target "$2" "$token" || return 1
    accepted=$(grep -c "Accepted publickey for $ME " "$T/sshd_pwtarget.log")
    run_built
    [ "$RC" -eq 0 ] || { REASON="the target exited $RC, expected 0: $(err_summary)"; return 1; }
    [ -e "$(marker_of "$token")" ] ||
        { REASON="the helper did not answer the target's prompt (askpass log: $(tr '\n' '~' <"$T/askpass.log"))"; return 1; }
    [ "$(prompts_asked)" -eq 1 ] && grep -qxF "prompt=$ME@$3's password: " "$T/askpass.log" ||
        { REASON="expected the target's prompt only, naming $ME@$3 (log: $(tr '\n' '~' <"$T/askpass.log"))"; return 1; }
    [ "$(grep -c "Accepted publickey for $ME " "$T/sshd_pwtarget.log")" -eq $((accepted + 1)) ] ||
        { REASON="the target's sshd did not let the connection in"; return 1; }

    # -O exit ends the master at once.
    built_command "$out" exit || return 1
    run_built_control
    [ "$RC" -eq 0 ] || { REASON="-O exit exited $RC: $(err_summary)"; return 1; }
    grep -qx 'Exit request sent.' <(tr -d '\r' <"$ERR") || { REASON="-O exit said: $(err_summary)"; return 1; }
    took=$(wait_gone "$MASTER_PID" "${PID_START[$MASTER_PID]}" "$sock" 1000) ||
        { REASON="after -O exit (process ms, socket ms): $took"; return 1; }
}

# The target's entry sends it through a ProxyJump of its own, to
# nowhere.invalid, and names its host key 127.0.0.1 while its host name is
# localhost: the connection works only if the profile's route (the
# ProxyCommand on the command line) wins over the configuration, and the
# prompt names the host key's alias.
check_connection_through_a_jump_host() {
    via_setup || return 1
    capture env PATH="$T/shim:$PATH" ssh -G -p "$TARGET_PORT" -- "$ME@$VIA_TARGET_HOST"
    grep -qx 'proxyjump nowhere.invalid' "$OUT" && grep -qx 'hostkeyalias 127.0.0.1' "$OUT" ||
        { REASON="precondition: the configuration does not send the target through nowhere.invalid"; return 1; }
    connect_through_jump_host via-db "$VIA_DB_ID" 127.0.0.1
}

# The target's entry has no route of its own and canonicalizes its host name:
# `ssh -G` for the profile as the configuration alone has it says
# localhost.localhost, but ssh canonicalizes only a connection without a proxy,
# so the connection through the jump host names localhost in its prompt. The
# helper answers that only if its own `ssh -G` carries a ProxyCommand as the
# connection does (Ruling T3-b).
check_jump_host_target_named_as_connected() {
    via_setup || return 1
    capture env PATH="$T/shim:$PATH" ssh -G -p "$TARGET_PORT" -- "$ME@$VIA_CANON_HOST"
    grep -qx 'hostname localhost.localhost' "$OUT" ||
        { REASON="precondition: ssh -G does not canonicalize $VIA_CANON_HOST to localhost.localhost (does this machine resolve *.localhost?): $(grep '^hostname ' "$OUT")"; return 1; }
    connect_through_jump_host via-canon "$VIA_CANON_ID" localhost
}

# A target that is an IPv6 address: the hop asks the master for `[::1]:PORT`
# (`-W ::1:PORT` would be refused by ssh; Ruling T3-a).
check_jump_host_ipv6_target() {
    via_setup || return 1
    port_speaks_ssh "$TARGET_PORT" ::1 ||
        { REASON="precondition: the target's sshd does not answer on [::1]:$TARGET_PORT"; return 1; }
    connect_through_jump_host via-v6 "$VIA_V6_ID" ::1
}

# ---------- syncs through a partner's jump host (issue #9) ----------
#
# `moniker-server sync` opens the jump host's shared connection itself, goes
# through it to the partner and ends it. A sync has no one-time token, so there
# is no marker that shows the helper answered a prompt. What shows it here:
#   * the askpass helper `moniker-server` uses is a wrapper that runs the real
#     helper as its child, hands its answer on to ssh and logs the profile, the
#     prompt and the helper's exit status (make_sync_tools), so the log has the
#     master's prompt and the partner's, in that order, each answered (exit 0)
#     and nothing else asked. A helper that refused would log exit 1, though
#     ssh would still go on with an empty password;
#   * the throwaway sshds refuse every password, as a user-level sshd must, and
#     log it ("Failed password"), and the key then signs in ("Accepted
#     publickey"). The jump host's sshd sees exactly one of each: the master's.
#     The hop goes through that connection, so it signs in to nothing, and the
#     partner's sshd sees exactly one: the sync's. That says how many logins
#     there were, not that a password was right (no user-level sshd can check
#     one); the helper's exit status says it answered.

SYNC_TOOLS=    # $T/synctools: a moniker-server whose askpass helper logs the prompts
PARTNER_ID=    # the sync partner's profile id on A

# The jump host's and the partner's host keys are in $T/kh, the known_hosts of
# the ssh shim: the sync pins StrictHostKeyChecking=yes, so nothing is added on
# first use.
trust_in_shim_kh() { # trust_in_shim_kh PORT
    printf '[127.0.0.1]:%s %s\n' "$1" "$(cat "$T/hostkey.pub")" >>"$T/kh"
}

# A copy of moniker-server in $SYNC_TOOLS, with a moniker-askpass beside it (the
# product looks for the helper next to its own executable). It runs the real
# helper as its child, so that the helper's exit status can be logged, leaves
# the helper's stdout (the answer) to ssh, and logs one line to
# $T/sync-askpass.log: the profile the helper was asked for, its exit status and
# the prompt (never the answer).
#
# The helper answers only an ssh whose parent is MONIKER_PID, read as the
# parent of its own parent, and with this wrapper between, its parent is the
# wrapper and the ssh is the grandparent (as in askpass-wrap.sh). So the
# wrapper makes the same decision one level down: only if the ssh's parent
# really is the MONIKER_PID the product set, the helper is told the ssh's id as
# MONIKER_PID; otherwise it keeps the product's value and refuses. What the
# helper decides after that (the profile, the identity ssh -G gives, the
# keyring) is its own.
make_sync_tools() {
    SYNC_TOOLS=$T/synctools
    mkdir -p "$SYNC_TOOLS" && cp -- "$BIN/moniker-server" "$SYNC_TOOLS/moniker-server" || return 1
    cat >"$SYNC_TOOLS/moniker-askpass" <<EOF
#!/bin/sh
prompt=\$1
stat=\$(cat "/proc/\$PPID/stat" 2>/dev/null)
stat=\${stat##*) }
set -- \$stat
if [ -n "\${MONIKER_PID-}" ] && [ "\${2-}" = "\$MONIKER_PID" ]; then MONIKER_PID=\$PPID; fi
export MONIKER_PID
"$BIN/moniker-askpass" "\$prompt"
code=\$?
printf 'profile=%s exit=%s prompt=%s\n' "\${MONIKER_PROFILE_ID-}" "\$code" "\$prompt" >>"$T/sync-askpass.log"
exit "\$code"
EOF
    chmod 700 "$SYNC_TOOLS/moniker-askpass"
}

jump_folder_empty() { [ -z "$(ls -A -- "$JUMP_DIR" 2>/dev/null)" ]; }

# How many lines of a throwaway sshd's log (NAME) match PATTERN.
sshd_log_count() { grep -c -- "$2" "$T/sshd_$1.log"; }

check_sync_through_a_jump_host() {
    local sync_port carried_a carried_b pw_a pw_b names sock
    local jump_failed jump_accepted partner_failed partner_accepted
    via_setup || return 1
    make_sync_tools || { REASON="cannot make the copy of moniker-server"; return 1; }
    wait_for 5 jump_folder_empty || { REASON="precondition: an earlier check left a socket in $JUMP_DIR"; return 1; }
    # The partner: a server whose PATH finds this build's moniker-server, so
    # that sync-serve runs as instance B; it takes a password and then the key.
    start_sshd pwsync "$BIN:$SYSTEM_PATH" both || { REASON="cannot start the partner's sshd"; return 1; }
    sync_port=$SSHD_PORT
    trust_in_shim_kh "$JUMP_PORT" && trust_in_shim_kh "$sync_port" || { REASON="cannot trust the host keys"; return 1; }
    PARTNER_ID=$(seed_password_profile sync-partner "$SYNC_PARTNER_HOST" "$sync_port" via-bastion) ||
        { REASON="cannot seed the partner's profile"; return 1; }
    # The configuration sends the partner through a jump host that does not
    # exist: the sync gets there only by the route on its command line.
    capture env PATH="$T/shim:$PATH" ssh -G -p "$sync_port" -- "$ME@$SYNC_PARTNER_HOST"
    grep -qx 'proxyjump nowhere.invalid' "$OUT" ||
        { REASON="precondition: the configuration does not send the partner through nowhere.invalid"; return 1; }
    # One new profile with a password on each side, to travel through it.
    pw_a="pw-$(random_token)" pw_b="pw-$(random_token)"
    carried_a=$(A "$HELPER" seed-profile --name carried-a --host carried-a.example.test --user alice) &&
        printf '%s\n' "$pw_a" | A "$HELPER" set-secret carried-a &&
        carried_b=$(B "$HELPER" seed-profile --name carried-b --host carried-b.example.test --user bob) &&
        printf '%s\n' "$pw_b" | B "$HELPER" set-secret carried-b ||
        { REASON="cannot seed the profiles to carry"; return 1; }
    jump_failed=$(sshd_log_count pwjump "Failed password for $ME ")
    jump_accepted=$(sshd_log_count pwjump "Accepted publickey for $ME ")
    partner_failed=$(sshd_log_count pwsync "Failed password for $ME ")
    partner_accepted=$(sshd_log_count pwsync "Accepted publickey for $ME ")

    : >"$T/sync-askpass.log"
    capture A env PATH="$T/shim:$PATH" setsid -w timeout 60 "$SYNC_TOOLS/moniker-server" sync sync-partner
    # Whatever happens, no master is left running (run_jump_check ends these).
    for sock in "$JUMP_DIR"/*; do [ ! -S "$sock" ] || JUMP_SOCKETS+=("$sock"); done
    [ "$RC" -eq 0 ] || { REASON="moniker-server sync exited $RC: $(err_summary)"; return 1; }

    # Both sides got what the other had, passwords included.
    names=$(B "$HELPER" list)
    grep -qxF carried-a <<<"$names" && grep -qxF sync-partner <<<"$names" ||
        { REASON="B does not list A's profiles (carried-a, sync-partner)"; return 1; }
    grep -qxF carried-b <<<"$(A "$HELPER" list)" || { REASON="A does not list carried-b"; return 1; }
    [ "$(secret_of B "$carried_a")" = "$pw_a" ] || { REASON="carried-a's password did not arrive on B"; return 1; }
    [ "$(secret_of A "$carried_b")" = "$pw_b" ] || { REASON="carried-b's password did not arrive on A"; return 1; }

    # The helper answered the master (the jump host's profile), then the
    # partner's ssh (the partner's profile), each with exit status 0, and
    # nothing else asked. A helper that refused would have logged exit=1.
    [ "$(wc -l <"$T/sync-askpass.log")" -eq 2 ] &&
        [ "$(sed -n 1p "$T/sync-askpass.log")" = "profile=$VIA_JUMP_ID exit=0 prompt=$ME@127.0.0.1's password: " ] &&
        [ "$(sed -n 2p "$T/sync-askpass.log")" = "profile=$PARTNER_ID exit=0 prompt=$ME@127.0.0.1's password: " ] ||
        { REASON="the helper did not answer exactly the master's prompt and then the partner's (log: $(tr '\n' '~' <"$T/sync-askpass.log"))"; return 1; }
    # One password sent and one key login at each sshd: the master's at the
    # jump host's, the sync's at the partner's. A hop that signed in by itself,
    # or a partner reached directly after the master, would show as another.
    [ "$(sshd_log_count pwjump "Failed password for $ME ")" -eq $((jump_failed + 1)) ] &&
        [ "$(sshd_log_count pwjump "Accepted publickey for $ME ")" -eq $((jump_accepted + 1)) ] ||
        { REASON="the jump host's sshd did not see exactly one password and one key login"; return 1; }
    [ "$(sshd_log_count pwsync "Failed password for $ME ")" -eq $((partner_failed + 1)) ] &&
        [ "$(sshd_log_count pwsync "Accepted publickey for $ME ")" -eq $((partner_accepted + 1)) ] ||
        { REASON="the partner's sshd did not see exactly one password and one key login"; return 1; }
    # The master was ended with the sync (it is signaled, not waited for).
    wait_for 5 jump_folder_empty || { REASON="a socket is left in $JUMP_DIR: $(ls -A "$JUMP_DIR" | tr '\n' ' ')"; return 1; }
}

# A sync whose connection to the jump host cannot be made fails: it never
# syncs with the partner directly instead. The partner's profile goes through
# a jump host, but its host name here is a plain alias of 127.0.0.1 that the
# partner's sshd answers directly, so a sync that skipped the jump host would
# succeed; and the sshds' logs show no login at all, at the jump host's or the
# partner's. The runtime folder (where the socket would go) is missing from the
# environment, not absolute, or open to others.
check_sync_without_a_runtime_folder_never_goes_direct() {
    local port before logs_before logs_after run why
    local msg="cannot connect through the jump host via-bastion: no private runtime folder for its shared connection"
    via_setup || return 1
    start_sshd pwnorun "$BIN:$SYSTEM_PATH" both || { REASON="cannot start the partner's sshd"; return 1; }
    port=$SSHD_PORT
    trust_in_shim_kh "$JUMP_PORT" && trust_in_shim_kh "$port" || { REASON="cannot trust the host keys"; return 1; }
    seed_password_profile no-runtime-partner "$OLD_HOST" "$port" via-bastion >/dev/null ||
        { REASON="cannot seed the partner's profile"; return 1; }
    mkdir -m 0777 "$T/open-run" && chmod 0777 "$T/open-run" || { REASON="cannot make a runtime folder open to others"; return 1; }
    # The partner can be reached directly (this also starts its first login).
    capture env PATH="$T/shim:$PATH" ssh -p "$port" -o StrictHostKeyChecking=yes -o BatchMode=yes -- "$ME@$OLD_HOST" true
    [ "$RC" -eq 0 ] || { REASON="precondition: the partner's sshd cannot be reached directly: $(err_summary)"; return 1; }
    before=$(state_snapshot) || { REASON="cannot read the state before the sync"; return 1; }
    logs_before=$(sshd_log_count pwjump 'Connection from\|Accepted\|Failed password')/$(sshd_log_count pwnorun 'Connection from\|Accepted\|Failed password')
    for run in unset relative open; do
        case $run in
        unset) capture A env -u XDG_RUNTIME_DIR PATH="$T/shim:$PATH" "${MONX[@]}" sync no-runtime-partner ;;
        relative) capture A env XDG_RUNTIME_DIR=run PATH="$T/shim:$PATH" "${MONX[@]}" sync no-runtime-partner ;;
        open) capture A env XDG_RUNTIME_DIR="$T/open-run" PATH="$T/shim:$PATH" "${MONX[@]}" sync no-runtime-partner ;;
        esac
        why="with XDG_RUNTIME_DIR $run"
        [ "$RC" -ne 0 ] || { REASON="exit status 0 $why, expected a failure"; return 1; }
        grep -qxF "moniker-server: $msg" "$ERR" || { REASON="unexpected message $why: $(err_summary)"; return 1; }
        [ ! -s "$OUT" ] || { REASON="a sync reported a result $why: $(tr '\n' ' ' <"$OUT")"; return 1; }
    done
    logs_after=$(sshd_log_count pwjump 'Connection from\|Accepted\|Failed password')/$(sshd_log_count pwnorun 'Connection from\|Accepted\|Failed password')
    [ "$logs_before" = "$logs_after" ] ||
        { REASON="a login reached an sshd (connections, jump host/partner, before $logs_before, after $logs_after)"; return 1; }
    jump_folder_empty || { REASON="a socket was made in $JUMP_DIR"; return 1; }
    unchanged_since "$before"
}

# A machine older than jump hosts turns a sync away while A holds a profile
# with a jump host (the HELLO says version 2), and nothing changes on either
# side. Its sync-serve is the helper's stand-in, which answers as the old code
# did: ERROR frame, code 2.
check_jump_hosts_keep_an_old_machine_out() {
    local before old_port
    via_setup || return 1
    grep -q '"jump_host"' "$(A "$HELPER" store-path)" ||
        { REASON="precondition: A holds no profile with a jump host"; return 1; }
    mkdir -p "$T/oldbin" &&
        printf '#!/bin/sh\nexec "%s" serve-version-1\n' "$HELPER" >"$T/oldbin/moniker-server" &&
        chmod 700 "$T/oldbin/moniker-server" || { REASON="cannot make the old moniker-server"; return 1; }
    start_sshd oldpartner "$T/oldbin:$SYSTEM_PATH" key || { REASON="cannot start the old machine's sshd"; return 1; }
    old_port=$SSHD_PORT
    trust_in_shim_kh "$old_port" || { REASON="cannot trust the host key"; return 1; }
    seed_password_profile old-machine "$OLD_HOST" "$old_port" >/dev/null || { REASON="cannot seed the old machine's profile"; return 1; }
    before=$(state_snapshot) || { REASON="cannot read the state before the sync"; return 1; }
    capture A env PATH="$T/shim:$PATH" "${MONX[@]}" sync old-machine
    [ "$RC" -ne 0 ] || { REASON="exit status 0, expected a failure"; return 1; }
    grep -qF "$MSG_PEER_TOO_OLD" "$ERR" || { REASON="unexpected message: $(err_summary)"; return 1; }
    unchanged_since "$before"
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
    run_check "askpass follows the ssh config and skips ProxyJump hops" check_askpass_follows_ssh_config
    run_check "host key confirmation is never auto-answered" check_host_key_not_auto_answered
    run_jump_check "jump connection: the master signs in as Moniker's child and leaves a socket" check_jump_master_signs_in
    run_jump_check "jump connection: the target is reached through the socket" check_target_through_the_socket
    run_jump_check "jump connection: a hop without the master never gets the stored password" check_hop_without_master
    run_jump_check "jump connection: the master ends at once on -O exit and on SIGTERM" check_master_ends_at_once
    run_jump_check "jump connection: a master nobody uses ends by itself" check_master_backstop
    run_jump_check "jump connection: no listening port" check_no_listening_port
    run_jump_check "jump connection: the product's commands reach the target through its jump host" check_connection_through_a_jump_host
    run_jump_check "jump connection: the helper names the target as the connection does, not canonicalized" check_jump_host_target_named_as_connected
    run_jump_check "jump connection: an IPv6 target is reached through the jump host" check_jump_host_ipv6_target
    run_jump_check "sync through a partner's jump host: converges, the helper answers both, no master is left" check_sync_through_a_jump_host
    run_jump_check "sync through a partner's jump host: no usable runtime folder fails and never syncs direct" check_sync_without_a_runtime_folder_never_goes_direct
    run_jump_check "sync: a machine older than jump hosts is kept out while a profile has one" check_jump_hosts_keep_an_old_machine_out

    say "== summary: $PASSED passed, $FAILED failed"
    [ "$FAILED" -eq 0 ]
}

if [ "${MONIKER_E2E_INNER:-}" = 1 ]; then
    inner_main
    exit $?
fi
outer_main
