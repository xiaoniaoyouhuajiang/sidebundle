# Install, build, and develop

Prefer the prebuilt musl static binaries from GitHub Releases when you want maximum portability.

## Install from GitHub Releases (recommended)

1. Download the musl static binary for your architecture:
   - `sidebundle-x86_64-musl`
   - `sidebundle-aarch64-musl`
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

## Cross-compile (musl)

```bash
cargo build --release --target x86_64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl
```

## Permissions and tracing

See `docs/permissions_en.md` and `docs/tracing_en.md`.

