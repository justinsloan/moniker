#!/usr/bin/env bash
# Build Debian packages from a release build, into target/debian/:
#   moniker-server  `moniker-server` and `moniker-askpass`, no GTK: all a
#                   headless sync target needs
#   moniker         the GTK4/libadwaita/VTE terminal app; depends on the
#                   moniker-server
#                   package of the same version, which puts `moniker-askpass`
#                   next to it in /usr/bin, and on libadwaita 1.9 and VTE 0.76
#                   or newer; it also carries the desktop file and the
#                   AppStream metainfo (data/), where the publisher shows;
#                   its postinst and prerm register /usr/bin/moniker as an
#                   x-terminal-emulator alternative at priority 20
# Library dependencies are read from the binaries by dpkg-shlibdeps. Needs
# cargo, dpkg-deb, dpkg-shlibdeps (dpkg-dev) and strip. Installs nothing.
set -euo pipefail
umask 022

cd "$(dirname "${BASH_SOURCE[0]}")/.."

for tool in cargo dpkg dpkg-deb dpkg-shlibdeps strip git; do
    command -v "$tool" >/dev/null || { echo "build-deb: missing tool: $tool" >&2; exit 2; }
done

# The publisher of both packages is fixed here ("Justin Sloan", the user's
# choice); only the address comes from git, so a Maintainer field always has one.
# The copyright line below is a different thing and keeps the middle initial.
PUBLISHER="Justin Sloan"
EMAIL=$(git config user.email) && [ -n "$EMAIL" ] ||
    { echo "build-deb: set git user.email (used in the Maintainer field)" >&2; exit 2; }
MAINTAINER="$PUBLISHER <$EMAIL>"
VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
[ -n "$VERSION" ] || { echo "build-deb: no version in Cargo.toml" >&2; exit 2; }
# The AppStream metadata of the moniker package. Its release list must carry
# this build's version, so a version bump cannot leave it behind.
METAINFO=data/org.moniker.Moniker.metainfo.xml
grep -qF "<release version=\"$VERSION\"" "$METAINFO" ||
    { echo "build-deb: $METAINFO has no release $VERSION; add one" >&2; exit 2; }
# The repository is the packages' Homepage and the metainfo's homepage.
REPOSITORY=$(sed -n 's/^repository = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
[ -n "$REPOSITORY" ] || { echo "build-deb: no repository in Cargo.toml" >&2; exit 2; }
grep -qF "<url type=\"homepage\">$REPOSITORY</url>" "$METAINFO" ||
    { echo "build-deb: $METAINFO's homepage is not $REPOSITORY" >&2; exit 2; }
ARCH=$(dpkg --print-architecture)
# Timestamps inside the .deb come from the last commit, so a rebuild of the
# same commit gives the same files.
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct)}

cargo build --release --workspace --locked

OUT=target/debian
STAGE=$OUT/stage
rm -rf "$STAGE"
mkdir -p "$STAGE"

# stage_bin PACKAGE BINARY: copy a stripped release binary into the package.
stage_bin() {
    install -D -m 755 "target/release/$2" "$STAGE/$1/usr/bin/$2"
    strip --strip-unneeded "$STAGE/$1/usr/bin/$2"
}

# shlib_depends PACKAGE: the Depends list for the package's binaries.
# dpkg-shlibdeps insists on a debian/control, so a throwaway one is made.
shlib_depends() {
    local dir=$STAGE/$1 bins=() b
    for b in "$dir"/usr/bin/*; do bins+=("-e${b#"$dir"/}"); done
    (
        cd "$dir"
        mkdir debian
        printf 'Source: moniker\n\nPackage: %s\nArchitecture: any\n' "$1" >debian/control
        dpkg-shlibdeps -O "${bins[@]}" 2>/dev/null | sed -n 's/^shlibs:Depends=//p'
        rm -r debian
    )
}

# with_floor DEPENDS PACKAGE MINIMUM: DEPENDS (a comma-separated list) with the
# entry for PACKAGE at least "PACKAGE (>= MINIMUM)", moved to the end. A
# minimum dpkg-shlibdeps derived from the symbols the binary uses is kept if it
# is higher than MINIMUM (so a newer symbol is never hidden by an older floor)
# and replaced if it is lower, which it can be for the library version the GUI
# is built and tested against; an entry that is not of the form
# "PACKAGE (>= VERSION)" is replaced.
with_floor() {
    local entry entries out=() keep="$2 (>= $3)" version
    IFS=',' read -ra entries <<<"$1"
    for entry in "${entries[@]}"; do
        entry=${entry# }
        if [ "${entry%% *}" != "$2" ]; then
            out+=("$entry")
        elif [[ $entry =~ ^"$2"\ \(\>=\ ([^\)]+)\)$ ]]; then
            version=${BASH_REMATCH[1]}
            if dpkg --compare-versions "$version" gt "$3"; then
                keep=$entry
            fi
        fi
    done
    out+=("$keep")
    local IFS=','
    sed 's/,/, /g' <<<"${out[*]}"
}

# stage_copyright PACKAGE: the package's copyright file: the owner, then the
# MIT license text from LICENSE. (The publisher above is only the Maintainer
# field and the metainfo's developer.)
stage_copyright() {
    {
        printf 'Moniker\nCopyright 2026 Justin M. Sloan\nLicense: MIT\n\n'
        cat LICENSE
    } | install -D -m 644 /dev/stdin "$STAGE/$1/usr/share/doc/$1/copyright"
}

# build_deb PACKAGE: write DEBIAN/control from the CONTROL_* variables and pack.
build_deb() {
    local dir=$STAGE/$1
    mkdir -p "$dir/DEBIAN"
    {
        printf 'Package: %s\n' "$1"
        printf 'Version: %s\n' "$VERSION"
        printf 'Architecture: %s\n' "$ARCH"
        printf 'Maintainer: %s\n' "$MAINTAINER"
        printf 'Installed-Size: %s\n' "$(du -sk --exclude=DEBIAN "$dir" | cut -f1)"
        printf 'Depends: %s\n' "$CONTROL_DEPENDS"
        [ -z "${CONTROL_RECOMMENDS:-}" ] || printf 'Recommends: %s\n' "$CONTROL_RECOMMENDS"
        printf 'Section: %s\n' "$CONTROL_SECTION"
        printf 'Priority: optional\n'
        printf 'Homepage: %s\n' "$REPOSITORY"
        printf 'Description: %s\n' "$CONTROL_DESCRIPTION"
    } >"$dir/DEBIAN/control"
    dpkg-deb --root-owner-group --build "$dir" "$OUT/${1}_${VERSION}_${ARCH}.deb" >/dev/null
    echo "$OUT/${1}_${VERSION}_${ARCH}.deb"
}

# ----- moniker-server -----
stage_bin moniker-server moniker-server
stage_bin moniker-server moniker-askpass
stage_copyright moniker-server
CONTROL_DEPENDS="$(shlib_depends moniker-server), openssh-client"
CONTROL_RECOMMENDS="gnome-keyring"
CONTROL_SECTION=net
CONTROL_DESCRIPTION="Moniker command line and sync server (no GUI)
 moniker-server syncs saved SSH connection profiles and their passwords
 between your own machines over plain ssh, encrypted with a shared sync key.
 Install it on every machine that takes part in a sync, including headless
 ones. Stored passwords live in the Secret Service (system keyring).
 Also contains moniker-askpass, the helper ssh calls for a stored password."
build_deb moniker-server

# ----- moniker -----
stage_bin moniker moniker
stage_copyright moniker
# The AppStream metadata: App Center and GNOME Software show the developer from it.
install -D -m 644 "$METAINFO" "$STAGE/moniker/usr/share/metainfo/org.moniker.Moniker.metainfo.xml"
# The desktop file, shared with scripts/build-rpm.sh.
install -D -m 644 data/org.moniker.Moniker.desktop "$STAGE/moniker/usr/share/applications/org.moniker.Moniker.desktop"
# The x-terminal-emulator alternative (issue #15): programs that start "the
# terminal" through Debian's mechanism find Moniker among the choices, and
# `update-alternatives --config x-terminal-emulator` picks it. Priority 20 never
# outranks a terminal at 40 or above (Ubuntu's Ptyxis is 40), so on Ubuntu
# installing Moniker does not change which terminal the system chooses by itself.
# Plain scripts, no #DEBHELPER# (nothing here is built by debhelper).
install -D -m 755 /dev/stdin "$STAGE/moniker/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
case "$1" in
configure)
    update-alternatives --install /usr/bin/x-terminal-emulator \
        x-terminal-emulator /usr/bin/moniker 20
    ;;
esac
exit 0
EOF
install -D -m 755 /dev/stdin "$STAGE/moniker/DEBIAN/prerm" <<'EOF'
#!/bin/sh
set -e
case "$1" in
remove | deconfigure)
    update-alternatives --remove x-terminal-emulator /usr/bin/moniker
    ;;
esac
exit 0
EOF
# The floors are the versions the GUI is built and tested against: libadwaita
# 1.9 (the `v1_9` feature of libadwaita) and VTE 0.76 (vte4's `v0_76`
# feature: the terminal's context menu model). dpkg-shlibdeps reads 0.75.0 for
# VTE from the symbols file, the development series that led to 0.76. There is
# no GTK floor here: it comes transitively from libadwaita's own dependency on
# GTK, so the derived libgtk-4-1 minimum stands as it is.
GUI_DEPENDS=$(with_floor "$(shlib_depends moniker)" libadwaita-1-0 1.9)
GUI_DEPENDS=$(with_floor "$GUI_DEPENDS" libvte-2.91-gtk4-0 0.76)
CONTROL_DEPENDS="$GUI_DEPENDS, moniker-server (= $VERSION)"
CONTROL_RECOMMENDS=""
CONTROL_SECTION=x11
CONTROL_DESCRIPTION="SSH terminal with saved connection profiles
 Moniker is a GTK4/libadwaita/VTE terminal built around saved SSH
 connections: one click opens a logged-in tab, optionally running a fixed
 startup sequence. Sessions can be saved as plain-text transcripts, and profiles
 can be synced between your own machines with no central server."
build_deb moniker
