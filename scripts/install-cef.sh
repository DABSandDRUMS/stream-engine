#!/usr/bin/env bash
# Build the CEF host `stream-engine-web` (crate se-web-host) and install it together with the
# CEF runtime where the engine looks for it:
#
#   ~/.local/share/stream-engine/cef/<cef version>/cef_linux_x86_64/   CEF distribution (download)
#   ~/.local/share/stream-engine/cef/bin/                              host + runtime (installed)
#
# The CEF "minimal" distribution matching the `cef` crate is downloaded once by cef-dll-sys's
# build script (CEF_PATH). The engine itself never links libcef; it finds the host via
# $STREAM_ENGINE_WEB_HOST, next to its own executable, <share>/../../lib/stream-engine/, or the
# bin/ directory above (docs/web.md).
#
# Usage: scripts/install-cef.sh [--debug] [--keep-archive]
#   --debug          install a debug build of the host (faster to build)
#   --keep-archive   keep the downloaded .tar.bz2 (≈300 MB) after extracting the license
# Environment: STREAM_ENGINE_CEF_DIR (default ~/.local/share/stream-engine/cef), CARGO_TARGET_DIR.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CEF_ROOT="${STREAM_ENGINE_CEF_DIR:-$HOME/.local/share/stream-engine/cef}"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
PROFILE=release
KEEP_ARCHIVE=0
for arg in "$@"; do
  case "$arg" in
    --debug) PROFILE=debug ;;
    --keep-archive) KEEP_ARCHIVE=1 ;;
    -h|--help) sed -n '2,17p' "$0"; exit 0 ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done

command -v cargo >/dev/null || export PATH="$HOME/.cargo/bin:$PATH"
command -v cargo >/dev/null || { echo "cargo not found (install Rust via rustup)" >&2; exit 1; }

# Runtime files next to the binary (everything else in the distribution is build-time only;
# chrome-sandbox is not used: the host runs with --no-sandbox).
RUNTIME_FILES=(
  libcef.so libEGL.so libGLESv2.so libvk_swiftshader.so libvulkan.so.1 vk_swiftshader_icd.json
  icudtl.dat v8_context_snapshot.bin chrome_100_percent.pak chrome_200_percent.pak resources.pak
)

echo "==> building se-web-host ($PROFILE) with CEF_PATH=$CEF_ROOT"
mkdir -p "$CEF_ROOT"
cargo_flags=(-p se-web-host)
[[ "$PROFILE" == release ]] && cargo_flags+=(--release)
(cd "$ROOT" && CEF_PATH="$CEF_ROOT" CARGO_TARGET_DIR="$TARGET_DIR" cargo build "${cargo_flags[@]}")

BUILT="$TARGET_DIR/$PROFILE/stream-engine-web"
[[ -x "$BUILT" ]] || { echo "build did not produce $BUILT" >&2; exit 1; }
# `--se-version` needs libcef.so, which cef-dll-sys copied next to the build output.
VERSION_LINE="$("$BUILT" --se-version)"
CEF_VERSION="$(sed -n 's/.*(CEF \([0-9][0-9.]*\)+.*/\1/p' <<<"$VERSION_LINE")"
[[ -n "$CEF_VERSION" ]] || { echo "cannot read the CEF version from: $VERSION_LINE" >&2; exit 1; }
DIST="$CEF_ROOT/$CEF_VERSION/cef_linux_x86_64"
[[ -f "$DIST/libcef.so" ]] || { echo "CEF distribution not found at $DIST" >&2; exit 1; }
echo "==> $VERSION_LINE"
echo "==> distribution: $DIST"

# CEF's BSD license lives only in the archive root; keep a copy beside the distribution.
LICENSE="$CEF_ROOT/$CEF_VERSION/LICENSE.cef.txt"
ARCHIVE="$(find "$CEF_ROOT/$CEF_VERSION" -maxdepth 1 -name 'cef_binary_*_minimal.tar.bz2' -print -quit)"
if [[ ! -f "$LICENSE" && -n "$ARCHIVE" ]]; then
  echo "==> extracting LICENSE.txt from $(basename "$ARCHIVE")"
  tar -xjf "$ARCHIVE" --wildcards --no-anchored -O '*/LICENSE.txt' >"$LICENSE"
fi
if [[ -n "$ARCHIVE" && "$KEEP_ARCHIVE" == 0 && -s "$LICENSE" ]]; then
  echo "==> removing the downloaded archive (the extracted distribution stays)"
  rm -f "$ARCHIVE"
fi

# Stage, then swap directories so a running host keeps its (still open) old files.
STAGE="$CEF_ROOT/bin.new"
rm -rf "$STAGE"
mkdir -p "$STAGE/locales"
for f in "${RUNTIME_FILES[@]}"; do
  install -m 0644 "$DIST/$f" "$STAGE/$f"
done
chmod 0755 "$STAGE"/*.so "$STAGE"/libvulkan.so.1
cp "$DIST"/locales/*.pak "$STAGE/locales/"
# The distribution's libcef.so carries 1.2 GB of symbols; the dynamic symbol table is all we need.
strip --strip-unneeded "$STAGE/libcef.so"
install -m 0755 "$BUILT" "$STAGE/stream-engine-web"
strip --strip-debug "$STAGE/stream-engine-web"
install -m 0644 "$DIST/CREDITS.html" "$STAGE/CREDITS.html"
[[ -s "$LICENSE" ]] && install -m 0644 "$LICENSE" "$STAGE/LICENSE.cef.txt"

echo "==> verifying the installed host"
"$STAGE/stream-engine-web" --se-version

BIN="$CEF_ROOT/bin"
if [[ -d "$BIN" ]]; then
  mv "$BIN" "$CEF_ROOT/bin.old"
fi
mv "$STAGE" "$BIN"
rm -rf "$CEF_ROOT/bin.old"

echo "==> installed $(du -sh "$BIN" | cut -f1) in $BIN"
echo "    host: $BIN/stream-engine-web"
echo "    a running engine picks it up automatically (health.cef turns pass once a web source is open)"
