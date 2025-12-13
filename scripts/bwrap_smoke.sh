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

gh_group_start() {
  local title="$1"
  if [[ "${GITHUB_ACTIONS:-}" == "true" ]]; then
    echo "::group::$title"
  else
    echo "===== $title ====="
  fi
}

gh_group_end() {
  if [[ "${GITHUB_ACTIONS:-}" == "true" ]]; then
    echo "::endgroup::"
  else
    echo "===== end ====="
  fi
}

dump_npm_debug() {
  local npm_out="$1"
  local module_dirs=(
    "$npm_out/payload/usr/share/nodejs/@npmcli/node_modules/walk-up-path"
    "$npm_out/payload/usr/share/nodejs/npm/node_modules/walk-up-path"
  )
  gh_group_start "debug npm bundle layout"
  echo "npm_out=$npm_out"
  echo "payload root: $npm_out/payload"
  echo "trace_backend=$TRACE_BACKEND"
  echo "ls payload/usr/share/nodejs (top-level):"
  ls -la "$npm_out/payload/usr/share/nodejs" 2>/dev/null | head -n 200 || true
  echo "find walk-up-path under payload/usr/share/nodejs:"
  find "$npm_out/payload/usr/share/nodejs" -maxdepth 3 -name 'walk-up-path*' -print 2>/dev/null | head -n 200 || true
  echo "find semver under payload/usr/share/nodejs:"
  find "$npm_out/payload/usr/share/nodejs" -maxdepth 3 -name 'semver*' -print 2>/dev/null | head -n 200 || true
  echo "inspect walk-up-path module dirs:"
  for d in "${module_dirs[@]}"; do
    echo "--- $d ---"
    if [[ ! -e "$d" ]]; then
      echo "missing: $d"
      continue
    fi
    ls -la "$d" 2>/dev/null | head -n 200 || true
    if [[ -f "$d/package.json" ]]; then
      echo "package.json (first 80 lines):"
      sed -n '1,80p' "$d/package.json" 2>/dev/null || true
    else
      echo "missing package.json at $d"
    fi
    echo "symlinks under $d (first 50):"
    find "$d" -maxdepth 3 -type l -print 2>/dev/null | head -n 50 || true
    echo "broken symlinks under $d (first 50):"
    find "$d" -xtype l -print 2>/dev/null | head -n 50 || true
  done
  echo "npm entry points:"
  for p in \
    "$npm_out/payload/usr/bin/npm" \
    "$npm_out/payload/usr/bin/npm-cli.js" \
    "$npm_out/payload/usr/share/nodejs/npm/bin/npm-cli.js" \
    ; do
    if [[ -e "$p" ]]; then
      echo "- $p"
      (ls -la "$p" && readlink "$p") 2>/dev/null || true
      head -n 3 "$p" 2>/dev/null || true
    fi
  done
  echo "symlinks under payload/usr/share/nodejs (first 50):"
  find "$npm_out/payload/usr/share/nodejs" -maxdepth 2 -type l -print 2>/dev/null | head -n 50 || true
  echo "broken symlinks under payload/usr/share/nodejs (first 50):"
  find "$npm_out/payload/usr/share/nodejs" -xtype l -print 2>/dev/null | head -n 50 || true
  echo "node module resolution trace (NODE_DEBUG=module; first 200 lines):"
  (env NODE_DEBUG=module "$npm_out/bin/npm" --version 2>&1 || true) | head -n 200
  gh_group_end
}

dump_pip3_debug() {
  local pip3_out="$1"
  gh_group_start "debug pip3 bundle layout"
  echo "pip3_out=$pip3_out"
  echo "payload root: $pip3_out/payload"
  echo "trace_backend=$TRACE_BACKEND"
  echo "find _pydecimal.py in payload:"
  find "$pip3_out/payload" -maxdepth 5 -name '_pydecimal.py' -print 2>/dev/null | head -n 50 || true
  echo "find _decimal*.so in payload:"
  find "$pip3_out/payload" -maxdepth 7 -name '_decimal*.so' -print 2>/dev/null | head -n 50 || true
  echo "find libmpdec.so* in payload:"
  find "$pip3_out/payload" -maxdepth 7 -name 'libmpdec.so*' -print 2>/dev/null | head -n 50 || true
  echo "pip3 entry point:"
  if [[ -e "$pip3_out/payload/usr/bin/pip3" ]]; then
    ls -la "$pip3_out/payload/usr/bin/pip3" 2>/dev/null || true
    head -n 3 "$pip3_out/payload/usr/bin/pip3" 2>/dev/null || true
  fi
  gh_group_end
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
    tmpdir="$java_out/data/tmp-classes"
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
    run_bundle "run java class" "$java_out/bin/java" -cp "/data/tmp-classes" Hello
  else
    echo "javac not found; skipping java class run"
  fi
else
  echo "java not found; skipping java test"
fi

# pip3 (shebang + Python runtime resources)
pip3_bin="${SB_PIP3_BIN:-$(command -v pip3 || true)}"
if [[ -n "$pip3_bin" ]]; then
  pip3_out="$OUT/pip3"
  echo "pip3 trace_backend=$TRACE_BACKEND"
  run_bundle "bundle pip3" "$cli" --log-level "$LOG_LEVEL" create \
    --from-host "$pip3_bin::trace=--version" \
    --name pip3 \
    --out-dir "$OUT" \
    --run-mode bwrap \
    --trace-backend "$TRACE_BACKEND"
  if ! run_bundle "run pip3" "$pip3_out/bin/pip3" --version; then
    dump_pip3_debug "$pip3_out"
    exit 1
  fi
else
  echo "pip3 not found; skipping pip3 test"
fi

# npm (shebang + Node runtime resources)
# TODO(test_cover): npm (Debian/Ubuntu-packaged) depends on a large /usr/share/nodejs tree that
# includes symlink-based module aliases. Today we can capture the real files, but not always the
# alias paths that Node resolves first (e.g. @npmcli/node_modules/*), which can cause missing
# entrypoints like index.js at runtime. Re-enable this test once we preserve runtime alias paths
# for non-ELF resources more reliably.
echo "npm smoke test disabled (TODO: preserve runtime alias paths for nodejs modules)"
