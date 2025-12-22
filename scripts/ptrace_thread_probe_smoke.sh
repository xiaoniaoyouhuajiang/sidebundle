#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ARCH="$(uname -m)"
OUT="${OUT:-$ROOT/target/ptrace-thread-probe-$ARCH}"
LOG_LEVEL="${SB_LOG_LEVEL:-info}"

mkdir -p "$OUT"

if [[ "$(id -u)" -ne 0 ]]; then
  if [[ -r /proc/sys/kernel/yama/ptrace_scope ]]; then
    scope="$(cat /proc/sys/kernel/yama/ptrace_scope || echo "")"
    if [[ "$scope" != "0" ]]; then
      echo "skip ptrace thread-probe smoke (yama ptrace_scope=$scope; requires root to follow threads)"
      exit 0
    fi
  fi
fi

if ! command -v python3 >/dev/null 2>&1; then
  echo "python3 not found; skipping ptrace thread-probe"
  exit 0
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

name="ptrace-thread-probe"
rm -rf "$OUT/$name"

probe_script="$OUT/thread_probe.py"
cat >"$probe_script" <<'PY'
import threading

def worker():
    with open("/etc/hosts", "rb") as handle:
        handle.read(1)

t = threading.Thread(target=worker)
t.start()
t.join()
PY

# This probes a file from a worker thread. Without ptrace thread-following, the
# open() happens in a different tid and will be missed by the tracer.
"$cli" --log-level "$LOG_LEVEL" create \
  --from-host "/bin/sh::trace=-c 'python3 \"$probe_script\"'" \
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
  echo "ptrace thread probe regression failed: /etc/hosts not present in manifest" >&2
  exit 1
fi

echo "ptrace thread probe smoke ok ($manifest)"
