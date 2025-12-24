# Trace backends

sidebundle collects dependencies in two layers:
1. Static analysis (ELF `DT_NEEDED`, shebang interpreter, etc.).
2. Optional runtime tracing to capture runtime-only dependencies (e.g. `dlopen`-loaded `.so` files or resource/config reads).

This page describes the `--trace-backend` / `--image-trace-backend` values. For permissions, see
`docs/permissions_en.md`.

## Backends

- `off`: static analysis only.
- `ptrace`: ptrace-based runtime tracing (Linux only).
- `fanotify`: fanotify-based file access tracing (Linux only).
- `combined`: ptrace + fanotify (Linux only).
- `auto`: prefers stronger tracing on Linux; no-op on unsupported OSes.
- Image-only: `agent` / `agent-combined` run tracing inside the container.

## Trace commands (`::trace=...`)

Use short, deterministic commands that exit (e.g. `-version`, `--help`). For language runtimes, prefer
explicit triggers (e.g. Python: `-c 'import encodings'`).

## Controlling trace size (Python/Node)

Trace output is based on runtime file access only. If the bundle grows unexpectedly, it usually
means the runtime scanned more files than you expected (e.g., pyenv or user site-packages).
For Python/Node, startup can scan large package trees, which inflates bundle size. Tips:

- Use a minimal virtual environment with only required packages.
- Set `PYTHONNOUSERSITE=1` to skip user site-packages.
- Prefer explicit entrypoints (`python -S` or `-c 'import ...'`) if the tool supports it.
- Narrow `PYTHONPATH` to only needed paths.

## Related docs

- Permissions: `docs/permissions_en.md`
- Special handling notes: `docs/special_handling.md`
- FAQ: `docs/faq.md`
