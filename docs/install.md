# 安装、源码构建与开发

本项目的目标是生成尽可能可迁移的 bundle。通常建议使用 GitHub Releases 提供的 musl 静态二进制进行打包与运行。

## 下载安装（推荐）

1. 从 GitHub Releases 下载与你的 CPU 架构匹配的静态二进制：
   - 常规版（不内嵌 bwrap）：`sidebundle-x86_64-musl` / `sidebundle-aarch64-musl`
   - embedded-bwrap 版（内嵌静态 bwrap）：`sidebundle-x86_64-musl-embedded-bwrap` / `sidebundle-aarch64-musl-embedded-bwrap`

   embedded-bwrap 版可在目标机没有安装 bwrap 时使用 `--run-mode bwrap`，但仍需要内核允许 unprivileged userns。详见 `docs/bwrap.md`。

2. （可选）校验 SHA256：

```bash
sha256sum -c sidebundle-*.sha256
```

3. 放到 `PATH` 中并验证：

```bash
chmod +x ./sidebundle-*-musl
./sidebundle-*-musl --help
./sidebundle-*-musl create --help
```

## 从源码构建（本机）

要求：
- Rust 1.74+（建议使用 rustup 管理 toolchain）
- Linux（sidebundle 运行与跟踪能力依赖 Linux 内核特性）

构建：

```bash
cargo build --release
```

安装到 `~/.cargo/bin`：

```bash
cargo install --path sidebundle-cli
```

`cargo build`（gnu target）嵌入的 launcher **不是**可迁移产物。检验矩阵 `true (gnu 开发构建)` 在 centos:7 / ubuntu 18.04 上是 `LAUNCH_FAIL` / **GLIBC**（P0-3）：launcher 动态链接了构建机 glibc。发布脚本（`.github/workflows/release.yml`）只发布 musl 二进制。可迁移 bundle 请用下一节的 musl target。

若本机已 `rustup target add x86_64-unknown-linux-musl` 且 `rust-lld` 可用，packager 会自动把嵌入的 launcher/shim 编成 musl，即使 CLI 本身是 gnu。否则 `create` 时会警告 launcher 不可迁移。可用 `SIDEBUNDLE_LAUNCHER_TARGET` 强制指定嵌入 launcher 的 target。

## 交叉编译（musl，推荐）

sidebundle 常用于“在 A 系统打包，搬到 B 系统运行”。可迁移构建必须使用 musl 静态产物（发布物与 CI 的默认路径）：

```bash
# x86_64
cargo build --release --target x86_64-unknown-linux-musl

# aarch64
cargo build --release --target aarch64-unknown-linux-musl
```

若你使用 zig 作为链接器（更省心），可参考 README 或项目的 `.cargo/config.toml`。

## 运行时依赖与环境准备

sidebundle 的“运行时”是指：打包后的 bundle 在目标机器上执行 `bin/<entry>`。

- `Host` 模式：通常无额外依赖（但更容易受宿主环境影响）。
- `Bwrap` 模式：需要系统安装 `bwrap`（bubblewrap），且内核/发行版允许 unprivileged user namespace；或使用 embedded-bwrap 版本（见 `docs/bwrap.md`）。
- `Chroot` 模式：通常需要 root 或等价权限（取决于实现与 mount 行为）。

跟踪（trace）阶段也可能需要额外权限，详见 `docs/permissions.md`。

## 开发者工作流

常用命令：

```bash
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace
```

日志级别：

```bash
sidebundle-cli --log-level debug create --help
```

## 常见问题入口

- 跟踪后端/权限矩阵：`docs/tracing.md`、`docs/permissions.md`
- 特殊场景备忘：`docs/special_handling.md`
- 常见报错排查：`docs/faq.md`
