#!/usr/bin/env bash
set -euo pipefail

# bwrap smoke tests for Node/Python/Java.
# Must be run in an environment where bwrap can create user/mount namespaces.
# If run on GitHub Actions, the workflow runs inside a privileged Docker container.
#
# Override defaults via env:
#   SB_NODE_BIN, SB_NODE_SHARE
#   SB_PY_BIN, SB_PY_STDLIB
#   SB_JAVA_BIN, JAVA_HOME
#   SB_CLI (path to sidebundle-cli)
#   OUT (output dir)
# Debug:
#   SB_DEBUG=1 enables bash tracing and extra diagnostics

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ARCH="$(uname -m)"
OUT="${OUT:-$ROOT/target/smoke-$ARCH}"
TRACE_BACKEND="${SB_TRACE_BACKEND:-combined}"
LOG_FILE="${SB_LOG:-$OUT/smoke.log}"
LOG_LEVEL="${SB_LOG_LEVEL:-info}"
mkdir -p "$OUT"
if [[ "${SB_QUIET:-0}" != "0" ]]; then
  mkdir -p "$(dirname "$LOG_FILE")"
  touch "$LOG_FILE"
fi

if [[ "${SB_DEBUG:-0}" != "0" ]]; then
  set -x
fi

if [[ "${SB_QUIET:-0}" != "0" ]]; then
  trap 'status=$?; if [[ $status -ne 0 ]]; then echo "smoke failed (exit $status), showing last 200 lines from $LOG_FILE"; tail -n 200 "$LOG_FILE" || true; fi' EXIT
fi

arch_lib_dir() {
  case "$ARCH" in
    x86_64) echo "/usr/lib/x86_64-linux-gnu" ;;
    aarch64|arm64) echo "/usr/lib/aarch64-linux-gnu" ;;
    *) echo "" ;;
  esac
}

arch_lib_dir_root() {
  case "$ARCH" in
    x86_64) echo "/lib/x86_64-linux-gnu" ;;
    aarch64|arm64) echo "/lib/aarch64-linux-gnu" ;;
    *) echo "" ;;
  esac
}

multiarch_symlinks() {
  # Some distros rely on /lib64 or /usr/lib64 symlinks; include them if they exist.
  for p in /lib64 /usr/lib64; do
    [[ -e "$p" ]] && echo "$p"
  done
}

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

ensure_bwrap() {
  if ! command -v bwrap >/dev/null 2>&1; then
    echo "bubblewrap (bwrap) not found in PATH" >&2
    exit 1
  fi
  # Minimal capability check: can we unshare user/mount and bind /?
  if ! bwrap --unshare-user --unshare-ipc --unshare-pid --ro-bind / / true 2>/dev/null; then
    echo "bwrap capability check failed (user/mount namespaces not available)" >&2
    exit 1
  fi
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
ensure_bwrap
arch_lib="$(arch_lib_dir)"
arch_root_lib="$(arch_lib_dir_root)"
arch_symlinks=($(multiarch_symlinks))

# Node
node_bin="${SB_NODE_BIN:-$(command -v node || true)}"
if [[ -n "$node_bin" ]]; then
  node_out="$OUT/node"
  node_share="${SB_NODE_SHARE:-/usr/share/nodejs}"
  node_copy=()
  [[ -d "$node_share" ]] && node_copy+=(--copy-dir "$node_share")
  echo "node trace_backend=$TRACE_BACKEND"
  run_bundle "bundle node" "$cli" --log-level "$LOG_LEVEL" create \
    --from-host "$node_bin" \
    --name node \
    --out-dir "$OUT" \
    --run-mode bwrap \
    --trace-backend "$TRACE_BACKEND" \
    "${node_copy[@]}"
  run_bundle "run node" "$node_out/bin/node" -e "console.log('smoke-node')"
else
  echo "node not found; skipping node test"
fi

# Python
py_bin="${SB_PY_BIN:-$(command -v python3 || true)}"
if [[ -n "$py_bin" ]]; then
  py_out="$OUT/python3"
  py_stdlib="${SB_PY_STDLIB:-$("$py_bin" - <<'PY'
import sysconfig
print(sysconfig.get_paths()['stdlib'])
PY
)}"
  echo "python trace_backend=$TRACE_BACKEND"
  run_bundle "bundle python" "$cli" --log-level "$LOG_LEVEL" create \
    --from-host "$py_bin::trace=-c 'import encodings;import sys;sys.exit(0)'" \
    --name python3 \
    --out-dir "$OUT" \
    --run-mode bwrap \
    --trace-backend "$TRACE_BACKEND" \
    --copy-dir "$py_stdlib"
  run_bundle "run python" "$py_out/bin/python3" - <<'PY'
import sys, encodings
print("smoke-python", sys.version.split()[0])
PY
else
  echo "python3 not found; skipping python test"
fi

# Java
java_bin="${SB_JAVA_BIN:-$(command -v java || true)}"
if [[ -n "$java_bin" ]]; then
  if [[ -n "${JAVA_HOME:-}" ]]; then
    java_home="$(readlink -f "$JAVA_HOME")"
  else
    resolved_java="$(readlink -f "$java_bin")"
    java_home="$(dirname "$(dirname "$resolved_java")")"
  fi
  sec_target="$(readlink -f "$java_home/conf/security/java.security" || true)"
  sec_src=""
  sec_dest="$java_home/conf/security"
  if [[ -n "$sec_target" && -f "$sec_target" ]]; then
    sec_src="$(dirname "$sec_target")"
  fi
  java_out="$OUT/java"
  copy_args=(--copy-dir "$java_home")
  [[ -n "$sec_src" ]] && copy_args+=(--copy-dir "$sec_src:$sec_dest")
  [[ -n "$arch_lib" && -d "$arch_lib" ]] && copy_args+=(--copy-dir "$arch_lib")
  [[ -n "$arch_root_lib" && -d "$arch_root_lib" ]] && copy_args+=(--copy-dir "$arch_root_lib")
  for link in "${arch_symlinks[@]}"; do
    copy_args+=(--copy-dir "$link")
  done
  # Ensure JDK private libs are discoverable during trace inside bwrap.
  java_ld_path="${java_home}/lib/jli:${java_home}/lib/server:${LD_LIBRARY_PATH:-}"
  echo "java resolved: java_bin=$java_bin java_home=$java_home arch_lib=$arch_lib arch_root_lib=$arch_root_lib symlinks=${arch_symlinks[*]}"
  echo "java trace_backend=$TRACE_BACKEND"
  run_bundle "bundle java" env "LD_LIBRARY_PATH=${java_ld_path}" "$cli" --log-level "$LOG_LEVEL" create \
    --from-host "$java_bin::trace=-version" \
    --name java \
    --out-dir "$OUT" \
    --run-mode bwrap \
    --trace-backend "$TRACE_BACKEND" \
    "${copy_args[@]}"
  echo "find libstdc++ in bundle (java):"
  find "$java_out/payload" -maxdepth 4 -name 'libstdc++.so*' -type f -print || true
  echo "ldd on bundled java (host perspective):"
  ldd "$java_out/bin/java" || true
  run_bundle "run java version+settings" "$java_out/bin/java" -XshowSettings:properties -version
  if command -v javac >/dev/null 2>&1; then
    tmpdir="$java_out/tmp-classes"
    rm -rf "$tmpdir"
    mkdir -p "$tmpdir"
    cat >"$tmpdir/Hello.java" <<'EOF'
public class Hello {
    public static void main(String[] args) {
        System.out.println("smoke-java");
        System.out.println(System.getProperty("java.home"));
        System.out.println(System.getProperty("java.version"));
    }
}
EOF
    javac -d "$tmpdir" "$tmpdir/Hello.java"
    run_bundle "run java class" "$java_out/bin/java" -cp "$tmpdir" Hello
  else
    echo "javac not found; skipping java class run"
  fi
else
  echo "java not found; skipping java test"
fi
