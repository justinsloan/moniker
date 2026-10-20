#!/usr/bin/env bash
# Install-test the packages in DIR (both .deb, or both .rpm) on a clean Ubuntu
# or Fedora system, as CI does in a fresh container of each:
#   * they install with their declared dependencies alone (recommended and
#     weak dependencies are left out, so a missing Depends/Requires shows),
#   * every library and symbol the three binaries need is found (`ldd -r`),
#   * `moniker-server` runs, and finds no profiles in a new config,
#   * the desktop file and the AppStream metainfo are where desktops look,
#   * the GUI starts and is still running after 10 seconds on a virtual display
#     (with no session bus, a throwaway HOME and no keyring, as the window tests).
# It installs packages as root: run it ONLY in a throwaway container, never on
# your own machine.
set -euo pipefail

dir=${1:?usage: install-test.sh DIR}
# Absolute, so apt-get takes "dist/x.deb" as a file and not as package "dist"
# at release "x.deb" (it reads only arguments starting with / or ./ as files).
dir=$(cd "$dir" && pwd)
[ "$(id -u)" = 0 ] || { echo "install-test: run as root, in a throwaway container" >&2; exit 2; }
[ -f /.dockerenv ] || [ -n "${GITHUB_ACTIONS:-}" ] ||
    { echo "install-test: refusing to run outside a container" >&2; exit 2; }

if command -v apt-get >/dev/null; then
    shopt -s nullglob
    packages=("$dir"/*.deb)
    [ ${#packages[@]} = 2 ] || { echo "install-test: expected two .deb in $dir" >&2; exit 2; }
    apt-get update -q
    DEBIAN_FRONTEND=noninteractive apt-get install -y -q --no-install-recommends "${packages[@]}"
    display_packages=(xvfb xauth)
    install_display() {
        DEBIAN_FRONTEND=noninteractive apt-get install -y -q --no-install-recommends "${display_packages[@]}"
    }
elif command -v dnf >/dev/null; then
    shopt -s nullglob
    packages=("$dir"/*.rpm)
    [ ${#packages[@]} = 2 ] || { echo "install-test: expected two .rpm in $dir" >&2; exit 2; }
    dnf install -y -q --setopt=install_weak_deps=False "${packages[@]}"
    # Fedora's gtk4 loads GL at run time (libepoxy) without requiring it, and
    # every desktop session pulls it in; without libGLESv2 any GTK4 app aborts.
    display_packages=(xorg-x11-server-Xvfb xorg-x11-xauth libglvnd-gles mesa-dri-drivers)
    install_display() {
        dnf install -y -q --setopt=install_weak_deps=False "${display_packages[@]}"
    }
else
    echo "install-test: neither apt-get nor dnf found" >&2
    exit 2
fi

fail() { echo "install-test: $*" >&2; exit 1; }

for bin in moniker moniker-server moniker-askpass; do
    [ -x "/usr/bin/$bin" ] || fail "/usr/bin/$bin is missing"
    if missing=$(ldd -r "/usr/bin/$bin" 2>&1 | grep -E 'not found|undefined symbol'); then
        fail "$bin: $missing"
    fi
done
echo "install-test: all libraries and symbols found"

moniker-server --version
T=$(mktemp -d)
trap 'rm -rf -- "$T"' EXIT
mkdir -m 700 "$T/run"
listed=$(env -i PATH=/usr/bin:/bin HOME="$T" XDG_CONFIG_HOME="$T/cfg" \
    MONIKER_KEYRING_SERVICE=moniker-install-test moniker-server profile list)
[ "$listed" = "No profiles." ] || fail "a new config listed: $listed"

for file in /usr/share/applications/org.moniker.Moniker.desktop \
    /usr/share/metainfo/org.moniker.Moniker.metainfo.xml; do
    [ -f "$file" ] || fail "$file is missing"
done

# The virtual display is installed only now, after the library check, so its
# own dependencies cannot stand in for one the packages forgot.
install_display
status=0
env -i PATH=/usr/bin:/bin HOME="$T" SHELL=/bin/sh \
    XDG_CONFIG_HOME="$T/cfg" XDG_DATA_HOME="$T/data" XDG_RUNTIME_DIR="$T/run" \
    GDK_BACKEND=x11 GTK_A11Y=none NO_AT_BRIDGE=1 \
    DBUS_SESSION_BUS_ADDRESS="unix:path=$T/nonexistent" \
    MONIKER_KEYRING_SERVICE=moniker-install-test MESA_SHADER_CACHE_DISABLE=true \
    timeout 10 xvfb-run -a moniker || status=$?
# timeout's 124: still running when the time was up, which is the pass.
[ "$status" = 124 ] || fail "moniker exited with status $status within 10 seconds"
echo "install-test: ok"
