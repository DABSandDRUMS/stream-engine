#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-or-later
#
# Build the stream-engine OBS plugin and install it for the current user:
#   ~/.config/obs-studio/plugins/stream-engine/bin/64bit/stream-engine.so
#   ~/.config/obs-studio/plugins/stream-engine/data/locale/en-US.ini
#
# usage: obs-plugin/install.sh [--no-tests] [--build-dir DIR] [--uninstall]
# System-wide packages use `cmake --install` instead (see docs/obs.md).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(dirname "$here")"
build="$repo/target-obs/obs-plugin"
tests=1
uninstall=0
while [ $# -gt 0 ]; do
  case "$1" in
    --no-tests) tests=0 ;;
    --build-dir) build="$2"; shift ;;
    --uninstall) uninstall=1 ;;
    -h|--help) sed -n '3,10p' "$0"; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
  shift
done

dest="${XDG_CONFIG_HOME:-$HOME/.config}/obs-studio/plugins/stream-engine"

if [ "$uninstall" = 1 ]; then
  rm -rf "$dest"
  echo "removed $dest (restart OBS to unload the plugin)"
  exit 0
fi

# cmake: PATH first, then the mise install used on the dev machine
if ! command -v cmake >/dev/null 2>&1; then
  for c in "$HOME"/.local/share/mise/installs/cmake/*/cmake-*/bin; do
    [ -x "$c/cmake" ] && PATH="$c:$PATH"
  done
fi
command -v cmake >/dev/null 2>&1 || { echo "cmake not found (install cmake or \`mise use -g cmake\`)" >&2; exit 1; }

cmake -S "$here" -B "$build" -DCMAKE_BUILD_TYPE=RelWithDebInfo -DSE_OBS_BUILD_TESTS="$([ "$tests" = 1 ] && echo ON || echo OFF)" >/dev/null
cmake --build "$build" -j "$(nproc)"
if [ "$tests" = 1 ]; then
  ctest --test-dir "$build" --output-on-failure
fi

mkdir -p "$dest/bin/64bit" "$dest/data"
# install via rename: a running OBS keeps its mapping of the old file intact
tmp="$dest/bin/64bit/.stream-engine.so.$$"
install -m 0755 "$build/stream-engine.so" "$tmp"
mv -f "$tmp" "$dest/bin/64bit/stream-engine.so"
rm -rf "$dest/data.new"
cp -r "$here/data" "$dest/data.new"
rm -rf "$dest/data"
mv "$dest/data.new" "$dest/data"
cp "$here/LICENSE" "$dest/LICENSE"

echo "installed $dest/bin/64bit/stream-engine.so"
if pgrep -x obs >/dev/null 2>&1; then
  echo "OBS is running: restart it to load the new plugin"
fi
