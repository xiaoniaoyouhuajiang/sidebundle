#!/bin/sh
set -eu

tarball="${1:-}"
if [ -z "$tarball" ] || [ ! -f "$tarball" ]; then
  echo "usage: $0 <bwrap-musl-<arch>.tar.zst>" >&2
  exit 2
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

tar --zstd -xf "$tarball" -C "$tmp"

cd "$tmp"

if [ ! -f "./bwrap" ] || [ ! -x "./bwrap" ]; then
  echo "missing or non-executable ./bwrap in $tarball" >&2
  exit 1
fi
if [ ! -f "./VERSION" ]; then
  echo "missing ./VERSION in $tarball" >&2
  exit 1
fi
if [ ! -d "./LICENSES" ]; then
  echo "missing ./LICENSES in $tarball" >&2
  exit 1
fi

top="$(ls -1A | sort)"
expected="$(printf '%s\n' LICENSES VERSION bwrap | sort)"
if [ "$top" != "$expected" ]; then
  echo "unexpected tarball layout" >&2
  echo "got:" >&2
  echo "$top" >&2
  echo "expected:" >&2
  echo "$expected" >&2
  exit 1
fi

if ! command -v file >/dev/null 2>&1; then
  echo "missing 'file' utility; install it to verify static linkage" >&2
  exit 1
fi
if ! command -v readelf >/dev/null 2>&1; then
  echo "missing 'readelf' utility; install binutils to verify ELF headers" >&2
  exit 1
fi

file ./bwrap | grep -Eqi 'statically linked|static-pie linked' || {
  echo "bwrap does not look fully static (expected static or static-pie)" >&2
  file ./bwrap >&2 ||:
  exit 1
}

# Fully static binaries should not have an interpreter (PT_INTERP).
readelf -l ./bwrap | grep -q 'INTERP' && {
  echo "bwrap has PT_INTERP; expected fully static binary" >&2
  readelf -l ./bwrap >&2 ||:
  exit 1
} ||:

./bwrap --version >/dev/null

./bwrap --help 2>/dev/null | grep -q -- '--seccomp' || {
  echo "bwrap --help does not mention --seccomp; expected seccomp support to be enabled" >&2
  ./bwrap --help >&2 ||:
  exit 1
}

echo "VERSION:"
sed -n '1,200p' ./VERSION

if [ "${VERIFY_BWRAP_RUN:-0}" = "1" ]; then
  echo "running minimal bwrap sanity check..."
  ./bwrap \
    --unshare-user \
    --unshare-pid \
    --die-with-parent \
    --ro-bind / / \
    --proc /proc \
    --dev /dev \
    /bin/sh -c 'true'
fi

echo "ok: verified $tarball"
