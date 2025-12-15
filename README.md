# sidebundle

English | [中文](README.zh.md)

[![CI](https://github.com/xiaoniaoyouhuajiang/sidebundle/actions/workflows/ci.yml/badge.svg)](https://github.com/xiaoniaoyouhuajiang/sidebundle/actions/workflows/ci.yml)
[![smoke-tests](https://github.com/xiaoniaoyouhuajiang/sidebundle/actions/workflows/smoke-test.yml/badge.svg)](https://github.com/xiaoniaoyouhuajiang/sidebundle/actions/workflows/smoke-test.yml)

sidebundle builds relocatable bundles from ELF executables (dynamic or static) and shebang scripts.
The CLI can collect entries from the host filesystem or from OCI images (Docker/Podman), trace
runtime-loaded files, and emit a portable bundle directory with launchers and a manifest.

Inspired by:
- https://github.com/intoli/exodus
- https://github.com/ValveSoftware/steam-runtime

![head](./statics/header_n.webp)

## Documentation
- [Install & build](docs/install_en.md)
- [CLI usage (advanced)](docs/usage_en.md)
- [Trace backends & principles](docs/tracing_en.md)
- [bwrap run mode & embedded-bwrap](docs/bwrap_en.md)
- [Permissions matrix](docs/permissions_en.md)
- [Special handling notes (CN)](docs/special_handling.md)
- [FAQ (CN)](docs/faq.md)

## Before & after
`scip-index` bundle size comparison
![compare](./statics/compares.png)

1.Basic Demo: No More Being Troubled by 'glibc_x not found' and 'libx.so: cannot open shared object'

https://github.com/user-attachments/assets/0b0b1e63-c268-4217-afb0-489168ec6ece

2.Image usage: Extracting a shebang script (javascript) and its underlying ELF dependency (node) from a Docker (or podman) image and running it perfectly in a completely different Linux environment.

https://github.com/user-attachments/assets/0d4f2ec8-2864-4a33-ab3f-e51773a10af2

## Install
- GitHub Releases (recommended): `docs/install_en.md`
- Build from source: `docs/install_en.md`

## Quick start (static binary)
Grab the prebuilt musl-linked binary from GitHub Releases (e.g. `sidebundle-x86_64-musl` or `sidebundle-aarch64-musl`). It runs on
any modern Linux without extra dependencies.

Note: Releases also ship `sidebundle-*-musl-embedded-bwrap`, which embeds a static `bwrap`. This lets you use `--run-mode bwrap` on
hosts without installing bubblewrap (user namespaces are still required). See `docs/bwrap_en.md`.

### Scenario A: Ship a Python script to machines without Python
Assume your script has a proper shebang (e.g. `#!/usr/bin/env python3`):

```bash
./sidebundle-x86_64-musl create \
  --name hello-py \
  --from-host "./examples/hello.py" \
  --out-dir bundles \
  --trace-backend combined \
  --log-level info
```

`hello-py/bin/hello.py` will run Python from the bundle, even on hosts without Python installed.

If you want a stricter relocatability check via bwrap on hosts without system bwrap, use the embedded build:

```bash
./sidebundle-x86_64-musl-embedded-bwrap create \
  --name hello-py \
  --from-host "./examples/hello.py" \
  --out-dir bundles \
  --run-mode bwrap \
  --trace-backend combined \
  --log-level info
```

### Scenario B: Extract `jq` from an Alpine image to run on Ubuntu

```bash
./sidebundle-x86_64-musl create \
  --name jq-alpine \
  --from-image "docker://alpine:3.20::/usr/bin/jq::trace=--version" \
  --out-dir bundles \
  --image-trace-backend agent-combined \
  --log-level info
```

The resulting `jq-alpine/bin/jq` is a portable launcher that uses the bundled libs; Docker is only
needed at build time.

For more details (run modes, CLI specs, tracing, troubleshooting), start from the docs links above.
