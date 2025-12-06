# sidebundle

English | [中文](README.zh.md)

sidebundle builds relocatable bundles from dynamically linked ELF binaries. The CLI can collect
executables from the host filesystem or from OCI images (Docker/Podman), trace the files that are
loaded at runtime, and emit a portable directory tree with launchers and a manifest.

![head](./statics/header_n.webp)

With sidebundle, you can:
* Automatically shrink Docker images and run the output on any Linux host without a container runtime.
* Bundle multiple ELF dependencies of an app/workflow into a single portable bundle.

## Before & after
`scip-index` bundle size comparison
![compare](./statics/compares.png)

1.Basic Demo: No More Being Troubled by 'glibc_x not found' and 'libx.so: cannot open shared object'

https://github.com/user-attachments/assets/0b0b1e63-c268-4217-afb0-489168ec6ece

2.Image usage: Extracting a shebang script (javascript) and its underlying ELF dependency (node) from a Docker (or podman) image and running it perfectly in a completely different Linux environment.

https://github.com/user-attachments/assets/0d4f2ec8-2864-4a33-ab3f-e51773a10af2

## Features
- Bundle binaries that live on the host or inside an OCI image.
- Resolve shared library dependencies statically and via runtime tracing (ptrace/fanotify).
- Merge multiple entry points into a single payload with deduplicated files.
- Support shebang scripts by resolving interpreters, bundling their dependencies, and scanning bash
  scripts for common helpers.
- Generate launch scripts that set `SIDEBUNDLE_ROOT`, `LD_LIBRARY_PATH`, and optionally invoke the
  original linker.
- Record every shipped artifact in `manifest.lock` for auditing and reproducible builds.

## Requirements
- Linux host (target binaries support `linux-x86_64` and `linux-aarch64` with musl). Runtime tracing uses ptrace and
  fanotify, so the process needs `CAP_SYS_PTRACE` (root or equivalent) to trace binaries that it does
  not own.
- Docker or Podman to materialize OCI images. The chosen engine must allow `--cap-add SYS_PTRACE`
  and `--security-opt seccomp=unconfined` for the tracing agent.
- Rust toolchain 1.74+ to build the CLI (older versions may also work, but are not tested).

## Build and install
```bash
# build in-place
cargo build --release

# or install the CLI into ~/.cargo/bin
cargo install --path sidebundle-cli
```

## Cross-compilation for multiple architectures
sidebundle supports building statically linked binaries for both x86_64 and ARM64 (aarch64) architectures using musl libc for maximum portability.

### CI builds (GitHub Actions)
The project's CI automatically builds release binaries for both architectures when tags are created. Binary naming convention:
- `sidebundle-x86_64-musl` for x86_64 Linux with musl
- `sidebundle-aarch64-musl` for ARM64 Linux with musl

### Local cross-compilation setup
To build for ARM64 locally, you need to install zig (which includes cross-compilation toolchains):

```bash
# Install zig (version 0.11.0 or compatible)
# Download from https://ziglang.org/download/ or use package manager

# Example: download and extract zig 0.11.0
wget https://ziglang.org/download/0.11.0/zig-linux-x86_64-0.11.0.tar.xz
tar -xf zig-linux-x86_64-0.11.0.tar.xz
export PATH="$PWD/zig-linux-x86_64-0.11.0:$PATH"

# Verify zig is available
zig version

# Build for ARM64
cargo build --release --target aarch64-unknown-linux-musl

# Build for x86_64 (optional, using zig for consistency)
cargo build --release --target x86_64-unknown-linux-musl
```

The project includes `.cargo/config.toml` that configures zig as the linker for musl targets.

### Using alternative cross-compilation toolchains
If you prefer traditional cross-compilation toolchains instead of zig:

```bash
# On Ubuntu/Debian
sudo apt-get install gcc-aarch64-linux-gnu

# Then build with custom linker (not required if using zig)
RUSTFLAGS="-C linker=aarch64-linux-gnu-gcc" \
  cargo build --release --target aarch64-unknown-linux-musl
```

### Development workflow
1. **Local development**: Use `cargo build --release` for native builds
2. **Cross-compilation testing**: Use `cargo build --release --target aarch64-unknown-linux-musl` with zig installed
3. **CI testing**:
   - Should create pull requests (PRs) from feature branches; CI runs on PRs to ensure changes pass lint and test checks
   - All changes should be reviewed before merging to maintain code quality

The CLI exposes contextual help:

```bash
sidebundle --help
sidebundle create --help
```

## Quick start (static binary)
Grab the prebuilt musl-linked binary from GitHub Releases (e.g. `sidebundle-x86_64-musl` or `sidebundle-aarch64-musl`). It runs on
any modern Linux without extra dependencies.

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

## Bundle layout
A successful build writes `target/bundles/<name>` (customizable via `--out-dir`). Each bundle looks
like this:

```
bundle-name/
  bin/                 # entry point launchers
  data/<sha256>        # deduplicated file store used for hard/symlinks
  payload/...          # actual ELF files placed at their runtime-relative paths
  resources/traced/... # files captured by runtime tracing
  manifest.lock        # JSON manifest describing every file that was shipped
```

Launchers export `SIDEBUNDLE_ROOT` and live in `bin/`. Invoke them directly (e.g.
`bundle-name/bin/my-entry`).

## CLI overview
```
sidebundle create [OPTIONS]
```

The command accepts any number of `--from-host` and `--from-image` declarations. At least one source
is required.

### Host entries
`--from-host SPEC` adds a binary from the local filesystem. The format is
`PATH[::trace=<command>]`:

- `PATH` can be absolute or relative.
- `::trace=<command>` is optional. The argument is parsed using shell-like rules and is executed
  during tracing to capture files opened via `dlopen`, configuration reads, etc.

Example:

```bash
sidebundle create \
  --name htop-bundle \
  --from-host '/usr/bin/htop::trace="/usr/bin/htop -n 1"'
```

### OCI image entries
`--from-image SPEC` references binaries inside a container image. The format is
`[backend://]IMAGE::/absolute/path[::trace=<command>]`.

- `backend` is optional. Use `docker` or `podman` to pin the provider. Without a prefix the CLI tries
  Docker first and falls back to Podman.
- `IMAGE` is any reference understood by the chosen engine (tags, digests, registries).
- `/absolute/path` must exist inside the container rootfs.
- `::trace=<command>` behaves like the host variant but executes inside the image.

Example:

```bash
sidebundle create \
  --name busybox-sh \
  --from-image 'docker://busybox:latest::/bin/sh::trace="/bin/sh -c \"ls /\""' \
  --image-trace-backend agent \
  --image-agent-cli 'docker'
```

#### Image build modes
- **Chroot/exported rootfs (default)**: the image rootfs is exported to a temporary directory and
  traced from the host using the selected backend (`ptrace`, `fanotify`, `combined`, etc). Requires
  host capabilities for tracing dynamic binaries.
- **Agent**: `--image-trace-backend agent` or `agent-combined` launches `sidebundle agent trace`
  inside the container to capture runtime files, then exports the rootfs for packaging. Choose a
  container engine with `--image-agent-cli`, override the mounted binary with `--image-agent-bin`,
  and keep artifacts via `--image-agent-keep-output/rootfs` for debugging.

### Trace backends
Runtime tracing is optional but recommended for dynamic binaries. Choose a backend with
`--trace-backend` (host) and `--image-trace-backend` (images, defaults to the host setting).

| Value    | Description |
|----------|-------------|
| `off`    | Disable runtime tracing. Only statically linked dependencies are collected. |
| `auto`   | Use the combined ptrace + fanotify backend on Linux, fall back to a no-op tracer on other OSes. |
| `ptrace` | Force the ptrace tracer (Linux only). |
| `fanotify` | Force the fanotify tracer (Linux only). |
| `combined` | Run ptrace+fanotify simultaneously (Linux only). |
| `agent-combined` | Agent mode for image inputs using the combined tracer inside the container. |
| `agent` | Special mode for image inputs; launches `sidebundle agent trace` inside the container to run traces from within the image. |

### Other useful flags
- `--name` controls the bundle directory name (default `bundle`).
- `--target` sets the target triple (`linux-x86_64` or `linux-aarch64`).
- `--out-dir` places bundles under a custom root instead of `target/bundles`.
- `--trace-root DIR` treats host paths as if they were relative to `DIR`, allowing you to feed files
  extracted from chroots or rootfs archives.
- `--image-backend` sets the default image provider (`auto`, `docker`, `podman`).
- `--image-agent-bin PATH` points to a specific `sidebundle` binary that will be bind-mounted into
  containers when using the agent backend (defaults to the current CLI binary).
- `--image-agent-cli CMD` overrides the container runtime command (e.g. `"sudo -n podman"`).
- `--image-agent-keep-output` preserves the temporary directories generated by the tracing agent for
  debugging.
- `--image-agent-keep-rootfs` preserves the exported container rootfs from agent runs for inspection.
- `--allow-gpu-libs` allows GPU/DRM-related libraries (e.g., libdrm, libnvidia) to be included
  instead of being filtered out.
- `--strict-validate` turns linker validation failures into build failures.
- `--copy-dir SRC[:DEST]` recursively copies a host directory into the bundle payload. If DEST is omitted, files are placed under `payload/` preserving the source directory structure.
- `--set-env KEY=VALUE` overrides environment variables in generated launchers (repeatable).
- `--run-mode MODE` sets the runtime execution mode for launchers (`Host`, `Bwrap`, or `Chroot`). Default is `Host`.
- `--log-level` adjusts logging (`error`, `warn`, `info`, `debug`, `trace`).

## Mixed host + image example
The CLI merges closures from any number of sources:

```bash
sidebundle create \
  --name demo \
  --from-host '/opt/tools/foo::trace="/opt/tools/foo --warmup"' \
  --from-image 'podman://registry/my/api:stable::/usr/bin/api-server' \
  --trace-backend combined \
  --image-trace-backend agent
```

The output bundle contains launchers for both `foo` and `api-server`, along with their merged
payload.

## Tracing inside containers
When `--image-trace-backend agent` is selected, the CLI copies the `sidebundle` binary into the
container and executes `sidebundle agent trace` with a JSON spec describing each command. The agent
writes a report with all traced files, which is then added to the main dependency closure.
Use `--image-trace-backend agent-combined` to request the combined tracer inside the container.
Pair with `--image-agent-keep-output` or `--image-agent-keep-rootfs` when debugging agent runs.

Advanced users can invoke the agent manually for debugging:

```bash
sidebundle agent trace \
  --rootfs /payload \
  --spec /tmp/spec.json \
  --output /tmp/trace \
  --trace-backend ptrace
```

## Running bundles
Each entry has a wrapper under `bin/`. The wrapper:

1. Resolves `BUNDLE_ROOT`.
2. Sets `SIDEBUNDLE_ROOT` for downstream tooling.
3. Ensures the linker and binary exist.
4. Exports an `LD_LIBRARY_PATH` that includes every directory listed in the plan.
5. Launches either the binary directly (static) or via the runtime linker (dynamic).

Ship the entire bundle directory to another host and execute `bin/<entry>`.

## Validation and debugging
- Validation: the CLI always runs `BundleValidator` after packaging. Combine with `--strict-validate`
  to fail on missing dependencies.
- Logging: pass `--log-level debug` to inspect resolver decisions, merge reports, and validator
  output.
- Manifest inspection: `manifest.lock` contains both copied dependencies (`files`) and traced
  artifacts (`traced_files`) with their digests for auditing.
- Traced payload: every runtime-only file is stored under `resources/traced`, preserving the original
  logical path so you can see what the trace engine actually touched.

## Special scenarios and tips

- **fanotify trace “hangs”**: fanotify monitors filesystem events. If a traced command appears stuck,
  it is often because another process is holding open file descriptors or watching the same tree.
  Check for other file monitors and ensure the traced command can exit cleanly; retry with a simpler
  trace or switch to `ptrace/combined`.
- **High-risk asset filtering**: sidebundle filters some GPU/DRM-related libraries (e.g., `libdrm`,
  `libnvidia*`) to avoid bundling host-specific drivers. If your workload needs these (e.g., ffmpeg
  using DRM/VAAPI/NVENC), use `--allow-gpu-libs` to include them and ensure the target environment
  has matching devices/drivers.
- **Trace parameters differ by host/image**:
  - Host: `--trace-backend` supports `off|auto|ptrace|fanotify|combined`.
  - Image: `--image-trace-backend` supports the above plus `agent|agent-combined`, which run trace
    inside the container. Agent modes require Docker/Podman with `SYS_PTRACE`/`SYS_ADMIN` and
    `seccomp=unconfined`.
  Choose `combined`/`agent-combined` to capture `dlopen`-loaded libs (e.g., JVM `libjava.so`).
- **Permissions matrix**: see `docs/permissions_en.md` for required capabilities and runtime modes.

## How sidebundle collects dependencies

```
Sources:
  - Host entries (--from-host)       -> Chroot/host resolver -> ELF/shebang parse -> static deps
  - Image entries (--from-image)     -> Export rootfs or run agent inside container

Static phase:
  Parse ELF (DT_NEEDED) / shebang -> resolve interpreter -> copy binaries/libs into payload

Runtime trace (optional):
  ptrace/fanotify/agent(-combined) -> collect exec/open paths -> promote ELF/resources into closure

Packaging:
  Deduplicate files -> link into payload/data -> write manifest.lock -> emit launchers
```

## Testing and development
Run the usual Rust workflow while hacking on the project:

```bash
cargo fmt
cargo clippy --all-targets
cargo test --workspace
```

Feel free to open issues or send patches if you find cases the closure builder fails to capture.
