#!/usr/bin/env bash
# Fail if moniker-core or moniker-cli (directly or transitively) depend on GTK, VTE or
# libadwaita. The CLI and core must run on headless machines that have no GTK installed.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

if ! tree="$(cargo tree -p moniker-core -p moniker-cli --target all --all-features --prefix none)"; then
    echo "check-no-gtk: 'cargo tree' failed" >&2
    exit 2
fi

# -w: match whole words, so gtk4, gtk4-sys, gdk4-sys, vte4 and libadwaita hit, but unrelated
# names do not.
if hits="$(grep -Ew 'gtk4|vte4|gdk4|libadwaita' <<<"$tree")"; then
    echo "check-no-gtk: moniker-core/moniker-cli must not depend on GTK, VTE or libadwaita, but found:" >&2
    sort -u <<<"$hits" >&2
    exit 1
fi

echo "check-no-gtk: ok"
