#!/usr/bin/env bash
# Build the Arch package from the current working tree, without installing it.
#
#   packaging/build-local.sh [--keep] [extra makepkg args...]
#
# Snapshots the tree (tracked + untracked, non-ignored files: `git ls-files -co --exclude-standard`)
# into the tarball name the PKGBUILD's release source expects, stages PKGBUILD + tarball in a temp
# dir, writes the tarball's checksum into the staged PKGBUILD, and runs `makepkg -f` there.
#
# Output: target-pkg/stream-engine-<ver>-<rel>-x86_64.pkg.tar.zst (gitignored). --keep leaves the
# staging dir in place (it is always kept when makepkg fails).
#
# Environment (all optional):
#   CARGO_TARGET_DIR      default target-pkg/cargo, reused between runs so rebuilds are incremental
#   CEF_PATH              default ~/.local/share/stream-engine/cef (scripts/install-cef.sh's cache;
#                         cef-dll-sys downloads CEF there on first use)
#   PKGDEST               default target-pkg
#   SE_PKG_ALLOW_MISSING  see PKGBUILD
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
keep=0
if [[ ${1:-} == --keep ]]; then
  keep=1
  shift
fi

read -r pkgname pkgver pkgrel < <(
  # shellcheck disable=SC1091
  source "$repo/packaging/PKGBUILD"
  printf '%s %s %s\n' "$pkgname" "$pkgver" "$pkgrel"
)
prefix="$pkgname-$pkgver"
out="$repo/target-pkg"
mkdir -p "$out"

stage="$(mktemp -d "${TMPDIR:-/tmp}/$pkgname-pkg.XXXXXX")"
finish() {
  local status=$?
  if ((status == 0 && keep == 0)); then
    rm -rf "$stage"
  else
    echo "staging dir kept: $stage" >&2
  fi
}
trap finish EXIT

# Snapshot: every listed path that exists (tracked files deleted in the working tree are skipped).
cd "$repo"
git ls-files -co --exclude-standard -z |
  while IFS= read -r -d '' f; do
    if [[ -e $f || -L $f ]]; then printf '%s\0' "$f"; fi
  done |
  tar --null -T - --transform "s,^,$prefix/," --owner=0 --group=0 --numeric-owner \
    -czf "$stage/$prefix.tar.gz"

cp packaging/PKGBUILD packaging/stream-engine.install "$stage/"
sum="$(sha256sum "$stage/$prefix.tar.gz" | cut -d' ' -f1)"
sed -i "s/^sha256sums=.*/sha256sums=('$sum')/" "$stage/PKGBUILD"
grep -qx "sha256sums=('$sum')" "$stage/PKGBUILD" || {
  echo "could not write the checksum into the staged PKGBUILD" >&2
  exit 1
}

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$out/cargo}"
export CEF_PATH="${CEF_PATH:-$HOME/.local/share/stream-engine/cef}"
export PKGDEST="${PKGDEST:-$out}"

# pacman only knows packages it installed. When the Rust toolchain comes from rustup (~/.cargo)
# and cmake from mise, `cargo`/`cmake` are on PATH but pacman reports them missing, so makepkg's
# dependency check would refuse to build. In that case skip the check (--nodeps) after confirming
# the tools really are on PATH; nothing gets installed either way.
nodeps=()
unsatisfied="$(
  # shellcheck disable=SC1091
  source "$stage/PKGBUILD"
  pacman -T "${depends[@]}" "${makedepends[@]}" || true
)"
if [[ -n $unsatisfied ]]; then
  for tool in cargo rustc cmake clang; do
    command -v "$tool" >/dev/null || {
      echo "$tool is not on PATH (pacman also lacks: $(echo $unsatisfied))" >&2
      exit 1
    }
  done
  echo "pacman does not provide: $(echo $unsatisfied) — toolchain is on PATH (rustup/mise), using makepkg --nodeps" >&2
  nodeps=(--nodeps)
fi

cd "$stage"
makepkg -f "${nodeps[@]}" "$@"

pkgfile="$PKGDEST/$pkgname-$pkgver-$pkgrel-$(uname -m).pkg.tar.zst"
[[ -f $pkgfile ]] || {
  echo "makepkg finished but $pkgfile is missing" >&2
  exit 1
}
echo "$pkgfile"
