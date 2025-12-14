# 安装、源码构建与开发

本项目的目标是生成尽可能可迁移的 bundle。通常建议使用 GitHub Releases 提供的 musl 静态二进制进行打包与运行。

## 下载安装（推荐）

1. 从 GitHub Releases 下载与你的 CPU 架构匹配的静态二进制：
   - `sidebundle-x86_64-musl`
   - `sidebundle-aarch64-musl`
2. 放到 `PATH` 中并验证：

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

## 交叉编译（musl，推荐）

sidebundle 常用于“在 A 系统打包，搬到 B 系统运行”。为了提高可迁移性，建议构建 musl 静态产物：

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
- `Bwrap` 模式：需要系统安装 `bwrap`（bubblewrap），且内核/发行版允许 unprivileged user namespace。
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

