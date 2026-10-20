#!/usr/bin/env bash
# Build RPM packages from a release build, into target/rpm/: the same two
# packages as scripts/build-deb.sh, for Fedora 44 or newer (the first Fedora
# with libadwaita 1.9):
#   moniker-server  `moniker-server` and `moniker-askpass`, no GTK: all a
#                   headless sync target needs
#   moniker         the GTK4/libadwaita/VTE terminal app; requires the
#                   moniker-server package of the same version, which puts
#                   `moniker-askpass` next to it in /usr/bin, and libadwaita 1.9
#                   and VTE 0.76 or newer; it also carries the desktop file and
#                   the AppStream metainfo (data/), where the publisher shows
# Library dependencies are read from the binaries by rpmbuild. Build it on the
# Fedora release the packages are for, as CI does in a fedora:44 container.
# Needs cargo, rpmbuild (rpm-build), strip and git. Installs nothing.
set -euo pipefail
umask 022

cd "$(dirname "${BASH_SOURCE[0]}")/.."

for tool in cargo rpmbuild strip git; do
    command -v "$tool" >/dev/null || { echo "build-rpm: missing tool: $tool" >&2; exit 2; }
done

# The publisher is the one of build-deb.sh: Vendor is the name, Packager the
# name with the address from git, as the .deb's Maintainer field.
PUBLISHER="Justin Sloan"
EMAIL=$(git config user.email) && [ -n "$EMAIL" ] ||
    { echo "build-rpm: set git user.email (used in the Packager field)" >&2; exit 2; }
PACKAGER="$PUBLISHER <$EMAIL>"
VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
[ -n "$VERSION" ] || { echo "build-rpm: no version in Cargo.toml" >&2; exit 2; }
# RPM versions cannot hold a hyphen; a pre-release would need a ~ instead.
[[ $VERSION =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] ||
    { echo "build-rpm: version $VERSION is not plain X.Y.Z" >&2; exit 2; }
# The same metainfo checks as build-deb.sh: this version is in the release
# list and the homepage is the repository.
METAINFO=data/org.moniker.Moniker.metainfo.xml
grep -qF "<release version=\"$VERSION\"" "$METAINFO" ||
    { echo "build-rpm: $METAINFO has no release $VERSION; add one" >&2; exit 2; }
REPOSITORY=$(sed -n 's/^repository = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
[ -n "$REPOSITORY" ] || { echo "build-rpm: no repository in Cargo.toml" >&2; exit 2; }
grep -qF "<url type=\"homepage\">$REPOSITORY</url>" "$METAINFO" ||
    { echo "build-rpm: $METAINFO's homepage is not $REPOSITORY" >&2; exit 2; }
# Timestamps inside the .rpm (and its build time) come from the last commit,
# so a rebuild of the same commit gives the same files.
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct)}

cargo build --release --workspace --locked

OUT=target/rpm
TOP=$OUT/build
rm -rf "$TOP"
mkdir -p "$TOP"
SPEC=$TOP/moniker.spec

# The spec takes its values from the --define options below, so the heredoc is
# quoted and the shell expands nothing in it. The binaries are the release
# build above, stripped as build-deb.sh strips them, so there is no debuginfo
# package. The floors are build-deb.sh's: the libadwaita and VTE versions the
# GUI is built and tested against; rpmbuild adds the libraries' sonames itself.
cat >"$SPEC" <<'EOF'
%global debug_package %{nil}

Name:           moniker
Version:        %{moniker_version}
Release:        1%{?dist}
Summary:        SSH terminal with saved connection profiles
License:        MIT
URL:            %{moniker_repository}
Vendor:         %{moniker_publisher}
Packager:       %{moniker_packager}
Requires:       moniker-server = %{version}-%{release}
Requires:       libadwaita >= 1.9
Requires:       vte291-gtk4 >= 0.76

%description
Moniker is a GTK4/libadwaita/VTE terminal built around saved SSH
connections: one click opens a logged-in tab, optionally running a fixed
startup sequence. Sessions can be saved as plain-text transcripts, and profiles
can be synced between your own machines with no central server.

%package -n moniker-server
Summary:        Moniker command line and sync server (no GUI)
Requires:       openssh-clients
Recommends:     gnome-keyring

%description -n moniker-server
moniker-server syncs saved SSH connection profiles and their passwords
between your own machines over plain ssh, encrypted with a shared sync key.
Install it on every machine that takes part in a sync, including headless
ones. Stored passwords live in the Secret Service (system keyring).
Also contains moniker-askpass, the helper ssh calls for a stored password.

%install
for bin in moniker moniker-server moniker-askpass; do
    install -D -m 755 %{moniker_root}/target/release/$bin %{buildroot}%{_bindir}/$bin
    strip --strip-unneeded %{buildroot}%{_bindir}/$bin
done
install -D -m 644 %{moniker_root}/data/org.moniker.Moniker.desktop \
    %{buildroot}%{_datadir}/applications/org.moniker.Moniker.desktop
install -D -m 644 %{moniker_root}/data/org.moniker.Moniker.metainfo.xml \
    %{buildroot}%{_metainfodir}/org.moniker.Moniker.metainfo.xml
cp %{moniker_root}/LICENSE LICENSE

%files
%license LICENSE
%{_bindir}/moniker
%{_datadir}/applications/org.moniker.Moniker.desktop
%{_metainfodir}/org.moniker.Moniker.metainfo.xml

%files -n moniker-server
%license LICENSE
%{_bindir}/moniker-server
%{_bindir}/moniker-askpass
EOF

rpmbuild -bb --quiet \
    --define "_topdir $PWD/$TOP" \
    --define "_rpmdir $PWD/$OUT" \
    --define "_build_name_fmt %%{NAME}-%%{VERSION}-%%{RELEASE}.%%{ARCH}.rpm" \
    --define "moniker_root $PWD" \
    --define "moniker_version $VERSION" \
    --define "moniker_repository $REPOSITORY" \
    --define "moniker_publisher $PUBLISHER" \
    --define "moniker_packager $PACKAGER" \
    --define "source_date_epoch_from_changelog 0" \
    --define "use_source_date_epoch_as_buildtime 1" \
    --define "clamp_mtime_to_source_date_epoch 1" \
    "$SPEC"

ls "$OUT"/moniker-*"$VERSION"-1*.rpm
