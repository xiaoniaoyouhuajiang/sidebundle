#!/usr/bin/env bash
set -euo pipefail

# Host-mode smoke tests: package common ELF and shebang tools on system A,
# then run them directly (simulating system B with same arch). This script
# uses sidebundle-cli in --run-mode host.
#
# Env overrides:
#   SB_CLI       path to sidebundle-cli (default: auto-detect/build)
#   OUT          output dir (default: target/host-smoke-$(uname -m))
#   SB_LOG_LEVEL sidebundle-cli log level (default: info)
#   SB_QUIET     if set to 1, capture logs to $OUT/host-smoke.log
#   SB_DEBUG     if set to 1, enables bash tracing

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ARCH="$(uname -m)"
OUT="${OUT:-$ROOT/target/host-smoke-$ARCH}"
LOG_FILE="${SB_LOG:-$OUT/host-smoke.log}"
LOG_LEVEL="${SB_LOG_LEVEL:-info}"
TRACE_BACKEND="${SB_TRACE_BACKEND:-auto}"

mkdir -p "$OUT"
if [[ "${SB_QUIET:-0}" != "0" ]]; then
  mkdir -p "$(dirname "$LOG_FILE")"
  : >"$LOG_FILE"
fi

if [[ "${SB_DEBUG:-0}" != "0" ]]; then
  set -x
fi

if [[ "${SB_QUIET:-0}" != "0" ]]; then
  trap 'status=$?; if [[ $status -ne 0 ]]; then echo "host smoke failed (exit $status), showing last 200 lines from $LOG_FILE"; tail -n 200 "$LOG_FILE" || true; fi' EXIT
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

run_bundle() {
  local name="$1"; shift
  local cmd=("$@")
  if [[ "${SB_QUIET:-0}" != "0" ]]; then
    echo "==> $name (quiet; logs -> $LOG_FILE)"
    {
      echo "===== $name ====="
      echo "\$ ${cmd[*]}"
      "${cmd[@]}"
      echo "===== end $name ====="
    } >>"$LOG_FILE" 2>&1
  else
    echo "==> $name: ${cmd[*]}"
    "${cmd[@]}"
  fi
}

cli="$(ensure_cli)"

# Common helper to bundle an ELF and run a simple command.
bundle_and_run_elf() {
  local bin="$1" name="$2"
  shift 2
  local args=("$@")
  if [[ ! -x "$bin" ]]; then
    echo "skip $name: $bin not found/executable"
    return
  fi
  local out="$OUT/$name"
  run_bundle "bundle $name" "$cli" --log-level "$LOG_LEVEL" create \
    --from-host "$bin" \
    --name "$name" \
    --out-dir "$OUT" \
    --run-mode host \
    --trace-backend "$TRACE_BACKEND"
  run_bundle "run $name" "$out/bin/$name" "${args[@]}"
}

# ELF candidates: prioritize basic + heavier deps (curl, node, ffmpeg, lldb, tar).
bundle_and_run_elf "/bin/ls" "ls" "--version"
bundle_and_run_elf "/usr/bin/curl" "curl" "--version"
# ffmpeg may depend on GPU/graphics driver libs (e.g. libdrm) that are filtered by default.
# Allow them in this smoke bundle to ensure the artifact runs on a clean target machine.
if [[ -x "/usr/bin/ffmpeg" ]]; then
  run_bundle "bundle ffmpeg" "$cli" --log-level "$LOG_LEVEL" create \
    --from-host "/usr/bin/ffmpeg" \
    --name "ffmpeg" \
    --out-dir "$OUT" \
    --run-mode host \
    --trace-backend "$TRACE_BACKEND" \
    --allow-gpu-libs
  run_bundle "run ffmpeg" "$OUT/ffmpeg/bin/ffmpeg" -version
else
  echo "skip ffmpeg: /usr/bin/ffmpeg not found/executable"
fi

# lldb embeds Python; include the host stdlib so filesystem encodings load on target machines.
if [[ -x "/usr/bin/lldb" ]]; then
  lldb_copy=()
  if command -v python3 >/dev/null 2>&1; then
    py_stdlib="$(
      python3 - <<'PY'
import sysconfig
print(sysconfig.get_paths().get("stdlib",""))
PY
    )"
    # Avoid copying non-system stdlib trees (e.g. pyenv/conda in $HOME), which can explode bundle size.
    if [[ -n "$py_stdlib" && -d "$py_stdlib" ]]; then
      case "$py_stdlib" in
        /usr/lib/*|/usr/local/lib/*)
          lldb_copy+=(--copy-dir "$py_stdlib")
          ;;
        *)
          echo "note: detected python stdlib at $py_stdlib (non-system); skipping copy-dir for lldb"
          ;;
      esac
    fi
  fi
  run_bundle "bundle lldb" "$cli" --log-level "$LOG_LEVEL" create \
    --from-host "/usr/bin/lldb" \
    --name "lldb" \
    --out-dir "$OUT" \
    --run-mode host \
    --trace-backend "$TRACE_BACKEND" \
    --set-env "PYTHONHOME=/usr" \
    "${lldb_copy[@]}"
  run_bundle "run lldb" "$OUT/lldb/bin/lldb" --version
else
  echo "skip lldb: /usr/bin/lldb not found/executable"
fi
bundle_and_run_elf "/usr/bin/node" "node" "-e" "console.log('host-smoke-node')"
bundle_and_run_elf "/bin/tar" "tar" "--version"

# Shebang: custom bash script
hello_script="$OUT/hello.sh"
cat >"$hello_script" <<'EOF'
#!/usr/bin/env bash
echo "host-shebang-hello"
EOF
chmod +x "$hello_script"
run_bundle "bundle hello.sh" "$cli" --log-level "$LOG_LEVEL" create \
  --from-host "$hello_script" \
  --name hello-sh \
  --out-dir "$OUT" \
  --run-mode host \
  --trace-backend "$TRACE_BACKEND"
run_bundle "run hello.sh" "$OUT/hello-sh/bin/hello.sh"

echo "host smoke completed (OUT=$OUT)"
