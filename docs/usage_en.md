# CLI usage (advanced)

This document is a concise English companion to `docs/usage.md` (which is the primary, more detailed version).

## Bundle layout (quick mental model)

```
bundle-name/
  bin/                 # launchers / entry points
  data/<sha256>        # deduplicated file store
  payload/...          # runtime path layout (absolute paths under bundle root)
  resources/traced/... # runtime-only files captured by tracing
  manifest.lock        # manifest for auditing / reproducibility
```

## Entry specs

- Host: `--from-host PATH[::trace=<command>]`
- Image: `--from-image [backend://]IMAGE::/abs/path[::trace=<command>]`

## Key flags

- `--run-mode host|bwrap|chroot`
- `--trace-backend off|auto|ptrace|fanotify|combined`
- `--copy-dir SRC[:DEST]`
- `--set-env KEY=VALUE` (repeatable)
- `--log-level info|debug|trace`

## Common runtime recipes

- Python: use an explicit trace trigger (e.g. `-c 'import encodings'`) and/or copy stdlib.
- Node/npm: distro layouts often need `/usr/share/nodejs`.
- Java: copy the JRE tree and required config (e.g. `conf/security`).

See `docs/usage.md` for full examples and troubleshooting links.

For bwrap specifics (including embedded-bwrap builds), see `docs/bwrap_en.md`.
