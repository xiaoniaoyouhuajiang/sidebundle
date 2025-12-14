#!/bin/sh
set -eu

# Build a fully static bubblewrap (bwrap) binary via Alpine + musl.
#
# Example:
#   mkdir -p outdir
#   podman run --rm \
#     -v ./references/build-static-bwrap.sh:/tmp/build-static-bwrap.sh:ro,Z \
#     -v ./outdir:/outdir:z \
#     alpine:latest \
#     /bin/sh /tmp/build-static-bwrap.sh
#
# Output:
#   /outdir/bwrap-musl-<arch>.tar.zst
#     - ./bwrap
#     - ./VERSION
#     - ./LICENSES/...

BWRAP_TAG="${BWRAP_TAG:-v0.11.0}"
OUTDIR="${OUTDIR:-/outdir}"

arch="$(uname -m)"
case "$arch" in
  x86_64|aarch64) ;;
  *)
    echo "unsupported arch: $arch (expected x86_64 or aarch64)" >&2
    exit 2
    ;;
esac

apk add --no-cache \
  bash \
  binutils \
  file \
  gcc \
  git \
  libcap-dev \
  libcap-static \
  libseccomp-dev \
  meson \
  musl-dev \
  pkgconf \
  tar \
  zstd

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

git clone --branch "$BWRAP_TAG" --depth 1 https://github.com/containers/bubblewrap.git "$workdir/bubblewrap"
cd "$workdir/bubblewrap"

git_commit="$(git rev-parse HEAD)"

meson setup _builddir \
  --prefix=/usr \
  -Dbuildtype=release \
  -Dstrip=true \
  -Dprefer_static=true \
  -Dc_link_args=-static-pie

# Best-effort: ensure a "seccomp" option exists and is enabled; fail hard if not available.
if meson configure _builddir 2>/dev/null | grep -qE '^[[:space:]]*seccomp[[:space:]]' ; then
  meson configure _builddir -Dseccomp=enabled
else
  echo "meson option 'seccomp' not found; cannot guarantee seccomp support" >&2
  exit 1
fi

meson compile -C _builddir

sysroot="$workdir/sysroot"
meson install --no-rebuild --destdir "$sysroot" -C _builddir

bwrap_bin="$sysroot/usr/bin/bwrap"
if [ ! -x "$bwrap_bin" ]; then
  echo "expected installed bwrap at $bwrap_bin, but it does not exist" >&2
  exit 1
fi

file "$bwrap_bin" | grep -qi 'statically linked' || {
  echo "bwrap does not look statically linked; build is not fully static" >&2
  file "$bwrap_bin" >&2 ||:
  exit 1
}

"$bwrap_bin" --version >/dev/null

pkg="$workdir/pkg"
mkdir -p "$pkg/LICENSES/bubblewrap"
cp -f "$bwrap_bin" "$pkg/bwrap"

{
  echo "bubblewrap_tag=$BWRAP_TAG"
  echo "bubblewrap_commit=$git_commit"
  echo "arch=$arch"
  echo "alpine_release=$(cat /etc/alpine-release 2>/dev/null || echo unknown)"
  echo "build_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
} > "$pkg/VERSION"

for f in COPYING LICENSE COPYING.LGPL COPYING.GPL; do
  if [ -f "$f" ]; then
    cp -f "$f" "$pkg/LICENSES/bubblewrap/$f"
  fi
done

mkdir -p "$OUTDIR"
tar --numeric-owner --zstd -cf "$OUTDIR/bwrap-musl-$arch.tar.zst" -C "$pkg" .
