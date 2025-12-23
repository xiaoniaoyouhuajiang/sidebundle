#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ARCH="$(uname -m)"
OUT="${OUT:-$ROOT/target/ptrace-probe-$ARCH}"
LOG_LEVEL="${SB_LOG_LEVEL:-info}"

mkdir -p "$OUT"

if [[ "$(id -u)" -ne 0 ]]; then
  if [[ -r /proc/sys/kernel/yama/ptrace_scope ]]; then
    scope="$(cat /proc/sys/kernel/yama/ptrace_scope || echo "")"
    if [[ "$scope" != "0" ]]; then
      echo "skip ptrace child-probe smoke (yama ptrace_scope=$scope; requires root to follow children)"
      exit 0
    fi
  fi
fi

ensure_cli() {
  if [[ -n "${SB_CLI:-}" ]]; then
    echo "$SB_CLI"
    return
  fi
  local candidates=(
    "$ROOT/target/${ARCH}-unknown-linux-musl/release/sidebundle-cli"
    "$ROOT/target/release/sidebundle-cli"
  )
  for c in "${candidates[@]}"; do
    if [[ -x "$c" ]]; then
      echo "$c"
      return
    fi
  done
  echo "building sidebundle-cli..."
  cargo build --release -p sidebundle-cli
  echo "$ROOT/target/release/sidebundle-cli"
}

cli="$(ensure_cli)"

name="ptrace-child-probe"
rm -rf "$OUT/$name"

# This probes a file in a child process via stat-like syscalls. Without ptrace child-following,
# probe-only dependencies can be missed (fanotify won't see them because there is no open()).
"$cli" --log-level "$LOG_LEVEL" create \
  --from-host "/bin/sh::trace=-c 'stat /etc/hosts >/dev/null'" \
  --name "$name" \
  --out-dir "$OUT" \
  --run-mode host \
  --trace-backend ptrace

manifest="$OUT/$name/manifest.lock"
if [[ ! -f "$manifest" ]]; then
  echo "missing manifest at $manifest" >&2
  exit 1
fi

if ! grep -q '"source": "/etc/hosts"' "$manifest"; then
  echo "ptrace child probe regression failed: /etc/hosts not present in manifest" >&2
  exit 1
fi

echo "ptrace child probe smoke ok ($manifest)"
