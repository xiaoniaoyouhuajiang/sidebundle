#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

PY="${SB_DEMO_PY:-}"
NODE="${SB_DEMO_NODE:-}"
JAVA="${SB_DEMO_JAVA:-}"
JAVA_SRC="${SB_DEMO_JAVA_SRC:-$ROOT/HelloSidebundle.java}"

if [[ -z "$PY" ]]; then
  PY="$(command -v python3 || true)"
fi
if [[ -z "$NODE" ]]; then
  NODE="$(command -v node || true)"
fi
if [[ -z "$JAVA" ]]; then
  JAVA="$(command -v java || true)"
fi

echo "==> python (numpy matrix multiply)"
if [[ -z "$PY" || ! -x "$PY" ]]; then
  echo "missing python (set SB_DEMO_PY or ensure python3 is on PATH)" >&2
  exit 1
fi
"$PY" - <<'PY'
import numpy as np

a = np.array([[1, 2, 3], [4, 5, 6]])
b = np.array([[7, 8], [9, 10], [11, 12]])
c = a @ b
print("A=\n", a)
print("B=\n", b)
print("A@B=\n", c)
PY

echo
echo "==> java (compile+run via source-file mode)"
if [[ -z "$JAVA" || ! -x "$JAVA" ]]; then
  echo "missing java (set SB_DEMO_JAVA or ensure java is on PATH)" >&2
  exit 1
fi
if [[ ! -f "$JAVA_SRC" ]]; then
  echo "missing java source at $JAVA_SRC (set SB_DEMO_JAVA_SRC)" >&2
  exit 1
fi
"$JAVA" "$JAVA_SRC"

echo
echo "==> node (print runtime info)"
if [[ -z "$NODE" || ! -x "$NODE" ]]; then
  echo "missing node (set SB_DEMO_NODE or ensure node is on PATH)" >&2
  exit 1
fi
"$NODE" -e 'console.log("hello from node"); console.log("node", process.version); console.log("platform", process.platform, process.arch);'
