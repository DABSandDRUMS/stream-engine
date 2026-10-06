#!/usr/bin/env bash
# Build the pinned Aitum release with the shared-encoder/output lifetime repair.
# Installs only the user plugin, never stream destinations, scenes, audio or themes.
set -euo pipefail
case "${1:-}" in
  --build-only) install_plugin=false ;;
  '') install_plugin=true ;;
  -h|--help) printf '%s\n' 'Usage: scripts/install-aitum.sh [--build-only]' 'Requires git, patch, cmake, C++ compiler, OBS development files, Qt6 and libcurl.'; exit 0 ;;
  *) echo "Unknown argument: $1" >&2; exit 2 ;;
esac
[[ $# -le 1 ]] || { echo 'Too many arguments' >&2; exit 2; }
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
revision=d7d1420731b024dc98a4704c34a715b49d91f053
check_off_air() {
  local mode live
  mode="$(streamctl get show.mode)"
  live="$(streamctl get twitch.stream.live)"
  [[ "$mode" == 'show.mode = offline' || "$mode" == 'show.mode = rehearsal' ]] &&
    [[ "$live" == 'twitch.stream.live = false' ]] || {
      echo 'Refusing plugin installation: engine unreachable or show not verified off air.' >&2
      return 1
    }
}
if $install_plugin; then check_off_air; fi
cache="${XDG_CACHE_HOME:-$HOME/.cache}"
mkdir -p "$cache"
work="$(mktemp -d "$cache/stream-engine-aitum.XXXXXX")"
echo "Building in $work"
git clone --depth 1 --branch 1.2.4 https://github.com/Aitum/obs-aitum-stream-suite.git "$work/source"
[[ "$(git -C "$work/source" rev-parse HEAD)" == "$revision" ]] || { echo 'Unexpected upstream revision' >&2; exit 1; }
patch --batch --forward -d "$work/source" -p1 < "$root/scripts/aitum-1.2.4-ownership.patch"
cmake -S "$work/source" -B "$work/build" -DCMAKE_BUILD_TYPE=RelWithDebInfo
cmake --build "$work/build" --parallel "${CMAKE_BUILD_PARALLEL_LEVEL:-4}"
plugin="$work/build/aitum-stream-suite.so"
[[ -s "$plugin" ]] || { echo 'Plugin build artifact missing' >&2; exit 1; }
if ! $install_plugin; then printf 'Built: %s\n' "$plugin"; exit 0; fi
check_off_air
base="${XDG_CONFIG_HOME:-$HOME/.config}/obs-studio/plugins/aitum-stream-suite"
mkdir -p "$base/bin/64bit" "$base/data"
if [[ -f "$base/bin/64bit/aitum-stream-suite.so" ]]; then
  cp --reflink=auto "$base/bin/64bit/aitum-stream-suite.so" "$work/previous-aitum-stream-suite.so"
fi
cp -a "$work/source/data/." "$base/data/"
install -m 644 "$work/source/LICENSE" "$base/data/LICENSE"
# Atomic replacement leaves any already-loaded plugin mapped to its old inode.
install -m 755 "$plugin" "$base/bin/64bit/aitum-stream-suite.so.new"
mv -f "$base/bin/64bit/aitum-stream-suite.so.new" "$base/bin/64bit/aitum-stream-suite.so"
printf 'upstream=%s\nrepair=shared-encoder-and-stopped-output-ownership\n' "$revision" > "$base/data/stream-engine-build.txt"
sha256sum "$base/bin/64bit/aitum-stream-suite.so" >> "$base/data/stream-engine-build.txt"
printf 'Installed repaired Aitum 1.2.4. Restart OBS off air to load it. Build and previous binary: %s\n' "$work"
