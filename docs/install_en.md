# Install, build, and develop

Prefer the prebuilt musl static binaries from GitHub Releases when you want maximum portability.

## Install from GitHub Releases (recommended)

1. Download the musl static binary for your architecture:
   - `sidebundle-x86_64-musl`
   - `sidebundle-aarch64-musl`
   - embedded bwrap builds:
     - `sidebundle-x86_64-musl-embedded-bwrap`
     - `sidebundle-aarch64-musl-embedded-bwrap`

   The embedded-bwrap binaries allow `--run-mode bwrap` without installing system bubblewrap (user namespaces are still required). See `docs/bwrap_en.md`.
2. Make it executable and check the help output:

```bash
chmod +x ./sidebundle-*-musl
./sidebundle-*-musl --help
./sidebundle-*-musl create --help
```

## Build from source

Requirements:
- Rust 1.74+
- Linux

```bash
cargo build --release
cargo install --path sidebundle-cli
```

A plain `cargo build` on a glibc host embeds a dynamically linked launcher. That bundle fails on older glibc (centos:7, Ubuntu 18.04) with `GLIBC_*.* not found` (verification matrix: `true (gnu)` / LAUNCH_FAIL / P0-3). Release artifacts are musl-only.

If `x86_64-unknown-linux-musl` (or the matching musl target) and `rust-lld` are installed, the packager builds the embedded launcher/shim for musl even when the CLI itself is a gnu build. Otherwise `create` warns. Override with `SIDEBUNDLE_LAUNCHER_TARGET`.

## Cross-compile (musl)

```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl
```

## Permissions and tracing

See `docs/permissions_en.md` and `docs/tracing_en.md`.
