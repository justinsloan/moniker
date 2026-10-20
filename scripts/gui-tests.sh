#!/usr/bin/env bash
# Run the GUI tests: the `#[ignore]`d ones in crates/moniker-gui/src/gui_tests/, which
# open real GTK windows and run real (fake-ssh) terminals. This is the ONLY way to run
# them: plain `cargo test` skips them, and running them against your own desktop
# session is exactly what must not happen.
#
# How it stays away from your session
#   * `env -i`: the tests start with an empty environment, so they see none of your
#     session variables (no WAYLAND_DISPLAY, DISPLAY, DBUS_SESSION_BUS_ADDRESS ...).
#   * HOME and the XDG directories point into a throwaway temp dir ($T), so the real
#     config, data and runtime directories are never read or written.
#   * The session bus address points at a socket that does not exist. This script
#     starts NO bus and NO keyring (so nothing can reach or replace yours), and the
#     tests use an in-memory secret store.
#   * The display is a private virtual one from xvfb-run (X11, no accessibility bus).
#   * It kills nothing. xvfb-run stops the Xvfb it started; $T is removed on exit.
#     (MESA_SHADER_CACHE_DISABLE keeps that Xvfb from writing a shader cache into $T
#     while it shuts down, after $T has been removed, which would leave a stray dir.)
#
# Usage: scripts/gui-tests.sh [extra test-binary arguments, e.g. a name filter]
# The exit status is the test binary's.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

if ! command -v xvfb-run >/dev/null 2>&1; then
    echo "gui-tests: xvfb-run not found (Debian/Ubuntu: apt install xvfb)" >&2
    exit 2
fi

# Build the test binary and find it. With json-render-diagnostics, cargo prints only its
# `compiler-artifact` (and similar) JSON messages on stdout, which is captured here; the
# compiler's warnings and errors are rendered as usual on stderr, together with cargo's
# progress, so a failed build shows why. The `moniker` bin's test artifact is the one
# whose profile has test=true.
if ! messages="$(cargo test -p moniker-gui --no-run --message-format=json-render-diagnostics)"; then
    echo "gui-tests: building the test binary failed" >&2
    exit 2
fi
if command -v jq >/dev/null 2>&1; then
    bin="$(jq -r 'select(.reason == "compiler-artifact"
                         and .target.name == "moniker"
                         and (.target.kind | index("bin"))
                         and .profile.test == true
                         and .executable != null) | .executable' <<<"$messages" \
        | tail -n 1 || true)"
else
    bin="$(grep '"reason":"compiler-artifact"' <<<"$messages" \
        | grep '"name":"moniker"' \
        | grep -E '"profile":\{[^}]*"test":true' \
        | grep -o '"executable":"[^"]*"' \
        | tail -n 1 | sed 's/^"executable":"//; s/"$//' || true)"
fi
if [[ -z "$bin" || ! -x "$bin" ]]; then
    echo "gui-tests: could not find the moniker test binary in cargo's output" >&2
    exit 2
fi

T="$(mktemp -d)"
if [[ -z "$T" || ! -d "$T" ]]; then
    echo "gui-tests: mktemp failed" >&2
    exit 2
fi
trap 'rm -rf -- "$T"' EXIT
mkdir -m 700 "$T/run"

env -i \
    PATH="$HOME/.cargo/bin:/usr/bin:/bin" \
    HOME="$T" \
    XDG_CONFIG_HOME="$T/cfg" \
    XDG_DATA_HOME="$T/data" \
    XDG_RUNTIME_DIR="$T/run" \
    GDK_BACKEND=x11 \
    GTK_A11Y=none \
    NO_AT_BRIDGE=1 \
    DBUS_SESSION_BUS_ADDRESS="unix:path=$T/nonexistent" \
    MONIKER_KEYRING_SERVICE=moniker-gui-test \
    MESA_SHADER_CACHE_DISABLE=true \
    xvfb-run -a "$bin" --ignored --test-threads=1 "$@"
