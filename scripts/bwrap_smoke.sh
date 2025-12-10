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

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ARCH="$(uname -m)"
OUT="${OUT:-$ROOT/target/smoke-$ARCH}"
mkdir -p "$OUT"

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
  echo "==> $name: ${cmd[*]}"
  "${cmd[@]}"
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
  run_bundle "bundle node" "$cli" create \
    --from-host "$node_bin" \
    --name node \
    --out-dir "$OUT" \
    --run-mode bwrap \
    --trace-backend off \
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
  run_bundle "bundle python" "$cli" create \
    --from-host "$py_bin::trace=-c 'import encodings;import sys;sys.exit(0)'" \
    --name python3 \
    --out-dir "$OUT" \
    --run-mode bwrap \
    --trace-backend off \
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
  run_bundle "bundle java" "$cli" create \
    --from-host "$java_bin::trace=-version" \
    --name java \
    --out-dir "$OUT" \
    --run-mode bwrap \
    --trace-backend off \
    "${copy_args[@]}"
  run_bundle "run java version+settings" "$java_out/bin/java" -XshowSettings:properties -version
  if command -v javac >/dev/null 2>&1; then
    tmpdir="$(mktemp -d)"
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
    rm -rf "$tmpdir"
  else
    echo "javac not found; skipping java class run"
  fi
else
  echo "java not found; skipping java test"
fi
