# sidebundle

中文 | [English](README.md)

sidebundle 用于从动态链接的 ELF 可执行文件构建可迁移的离线包。CLI 可以从宿主机或 OCI 镜像（Docker/Podman）采集可执行文件，静态解析和运行时跟踪它们加载的文件，并生成可移植的目录结构（附启动器和 manifest）。

![head](./statics/header_n.webp)

依靠sidebundle，你能：
* 自动最小化Docker镜像，且无需runc等runtime即可在任意linux上运行
* 将软件/工作流依赖的多个elf打包成一个可迁移的bundle

## 效果演示
scip-index应用打包前后占用空间对比
![对比](./statics/compares.png)

1.基础演示：不再被‘glibc_x not found’和'libx.so: cannot open shared object'困扰

https://github.com/user-attachments/assets/0b0b1e63-c268-4217-afb0-489168ec6ece

2.镜像用法：从docker（或者是podman）镜像中提取shebang script(javascript)和背后的elf依赖（node）到另一个完全不同的linux环境下完美运行

https://github.com/user-attachments/assets/0d4f2ec8-2864-4a33-ab3f-e51773a10af2

## 特性
- 打包来自宿主机或 OCI 镜像的可执行文件。
- 通过静态分析与运行时跟踪（ptrace/fanotify）解析依赖。
- 将多个入口合并为一个去重后的 payload。
- 支持 shebang 脚本，解析解释器、打包其依赖，并对 bash 脚本做常见调用的静态扫描。
- 生成设置 `SIDEBUNDLE_ROOT`、`LD_LIBRARY_PATH` 的启动脚本，可选通过原始 linker 启动。
- 在 `manifest.lock` 记录所有发布的文件，便于审计和可复现构建。

## 运行环境要求
- 仅支持 Linux 主机（目标二进制支持 `linux-x86_64` 与 `linux-aarch64`，使用 musl）。运行时跟踪使用 ptrace/fanotify，若跟踪非自身进程需要 `CAP_SYS_PTRACE`（root 或等价权限）。
- Docker 或 Podman 用于拉取/展开 OCI 镜像，需允许 `--cap-add SYS_PTRACE` 和 `--security-opt seccomp=unconfined` 供跟踪代理使用。
- Rust 1.74+ 用于构建 CLI（更旧版本可能可用但未测试）。

## 构建与安装
```bash
# 本地构建
cargo build --release

# 安装到 ~/.cargo/bin
cargo install --path sidebundle-cli
```

## 多架构交叉编译
sidebundle 支持为 x86_64 和 ARM64 (aarch64) 架构构建 musl 静态链接的二进制文件，以获得最佳可移植性。

### CI 构建 (GitHub Actions)
项目 CI 在创建 tag 时会自动为两种架构构建 release 二进制文件。二进制命名约定：
- `sidebundle-x86_64-musl`：x86_64 Linux with musl
- `sidebundle-aarch64-musl`：ARM64 Linux with musl

### 本地交叉编译配置
如需本地构建 ARM64 版本，需要安装 zig（包含交叉编译工具链）：

```bash
# 安装 zig（版本 0.11.0 或兼容版本）
# 从 https://ziglang.org/download/ 下载或使用包管理器

# 示例：下载并解压 zig 0.11.0
wget https://ziglang.org/download/0.11.0/zig-linux-x86_64-0.11.0.tar.xz
tar -xf zig-linux-x86_64-0.11.0.tar.xz
export PATH="$PWD/zig-linux-x86_64-0.11.0:$PATH"

# 验证 zig 可用
zig version

# 构建 ARM64 版本
cargo build --release --target aarch64-unknown-linux-musl

# 构建 x86_64 版本（可选，使用 zig 保持一致性）
cargo build --release --target x86_64-unknown-linux-musl
```

项目包含 `.cargo/config.toml` 配置文件，将 zig 设置为 musl 目标的链接器。

### 使用其他交叉编译工具链
如果倾向于使用传统交叉编译工具链而非 zig：

```bash
# Ubuntu/Debian
sudo apt-get install gcc-aarch64-linux-gnu

# 使用自定义链接器构建（使用 zig 时无需此步骤）
RUSTFLAGS="-C linker=aarch64-linux-gnu-gcc" \
  cargo build --release --target aarch64-unknown-linux-musl
```

### 开发工作流
1. **本地开发**：使用 `cargo build --release` 进行原生构建
2. **交叉编译测试**：安装 zig 后使用 `cargo build --release --target aarch64-unknown-linux-musl`
3. **CI 测试**：
   - 应从功能分支创建拉取请求 (PR)；CI 会在 PR 上运行以确保更改通过 lint 和测试检查
   - 所有更改在合并前应经过评审以保持代码质量

CLI 提供上下文帮助：

```bash
sidebundle --help
sidebundle create --help
```

## 快速开始（使用 musl 静态二进制）
从 GitHub Releases 获取预编译的 musl 静态版（例如 `sidebundle-x86_64-musl` 或 `sidebundle-aarch64-musl`），无需额外依赖即可运行。

### 场景 A：打包 Python 脚本到无 Python 的机器
确保脚本带有正确的 shebang（如 `#!/usr/bin/env python3`），然后：

```bash
./sidebundle-x86_64-musl create \
  --name hello-py \
  --from-host "./examples/hello.py" \
  --out-dir bundles \
  --trace-backend combined \
  --log-level info
```

得到的 `hello-py/bin/hello.py` 会使用 bundle 内的 Python，在未安装 Python 的主机上也能运行。

### 场景 B：从 Alpine 镜像提取 `jq` 在 Ubuntu 上运行

```bash
./sidebundle-x86_64-musl create \
  --name jq-alpine \
  --from-image "docker://alpine:3.20::/usr/bin/jq::trace=--version" \
  --out-dir bundles \
  --image-trace-backend agent-combined \
  --log-level info
```

输出的 `jq-alpine/bin/jq` 为可移植启动器，运行时不再依赖 Docker（仅构建时需要）。

## Bundle 布局
构建成功后会写入 `target/bundles/<name>`（可通过 `--out-dir` 自定义）：

```
bundle-name/
  bin/                 # 启动器
  data/<sha256>        # 去重后的文件存储（硬/软链指向这里）
  payload/...          # 按运行时路径放置的 ELF 文件
  resources/traced/... # 运行时跟踪捕获的文件
  manifest.lock        # 描述所有发布文件的 JSON manifest
```

启动器位于 `bin/`，会导出 `SIDEBUNDLE_ROOT`。直接执行即可（例如 `bundle-name/bin/my-entry`）。

## CLI 概览
```
sidebundle create [OPTIONS]
```

命令接受任意数量的 `--from-host` 与 `--from-image`，至少提供一个。

### 宿主机入口
`--from-host SPEC` 添加宿主机上的二进制，格式 `PATH[::trace=<command>]`：

- `PATH` 可以是绝对或相对路径。
- `::trace=<command>` 可选。使用 shell 风格解析，在跟踪时执行，以捕获 `dlopen` / 配置文件等。

示例：

```bash
sidebundle create \
  --name htop-bundle \
  --from-host '/usr/bin/htop::trace="/usr/bin/htop -n 1"'
```

### OCI 镜像入口
`--from-image SPEC` 引用镜像内的二进制，格式 `[backend://]IMAGE::/absolute/path[::trace=<command>]`：

- `backend` 可选。用 `docker` 或 `podman` 固定提供者，省略则优先 Docker、回退 Podman。
- `IMAGE` 为镜像引用（tag/digest/registry）。
- `/absolute/path` 必须存在于镜像 rootfs 中。
- `::trace=<command>` 与宿主机类似，但在镜像内执行。

示例：

```bash
sidebundle create \
  --name busybox-sh \
  --from-image 'docker://busybox:latest::/bin/sh::trace="/bin/sh -c \"ls /\""' \
  --image-trace-backend agent \
  --image-agent-cli 'docker'
```

#### 镜像构建模式
- **导出 rootfs / chroot（默认）**：先将镜像 rootfs 导出到本地目录，再在宿主机上按所选跟踪
  后端（`ptrace`/`fanotify`/`combined` 等）进行解析。对动态二进制需要宿主机具备相应跟踪权限。
- **Agent 模式**：`--image-trace-backend agent` 或 `agent-combined` 会在容器内执行
  `sidebundle agent trace` 捕获运行时文件，然后导出 rootfs 进行打包。可用 `--image-agent-cli`
  选择容器引擎，`--image-agent-bin` 覆盖挂载的可执行文件，`--image-agent-keep-output/rootfs`
  便于调试。

### 跟踪后端
运行时跟踪可选，但推荐对动态二进制启用。通过 `--trace-backend`（宿主）与 `--image-trace-backend`（镜像，默认同宿主设置）指定。

| 值 | 说明 |
|----|------|
| `off` | 关闭运行时跟踪，仅收集静态依赖。 |
| `auto` | Linux 上使用组合 ptrace+fanotify，其他 OS 降级为 no-op。 |
| `ptrace` | 强制 ptrace 跟踪（仅 Linux）。 |
| `fanotify` | 强制 fanotify 跟踪（仅 Linux）。 |
| `combined` | 同时运行 ptrace+fanotify（仅 Linux）。 |
| `agent-combined` | 镜像输入的 agent 模式，在容器内使用组合跟踪。 |
| `agent` | 镜像输入的特殊模式：在容器内执行 `sidebundle agent trace`。 |

### 其他常用参数
- `--name`：bundle 目录名，默认 `bundle`。
- `--target`：目标三元组（`linux-x86_64` 或 `linux-aarch64`）。
- `--out-dir DIR`：将 bundle 写入自定义根目录。
- `--trace-root DIR`：将宿主路径视为相对 DIR，便于使用 chroot/rootfs 内容。
- `--image-backend`：默认镜像提供者（`auto`、`docker`、`podman`）。
- `--image-agent-bin PATH`：指定挂载到容器的 `sidebundle` 二进制（默认当前 CLI）。
- `--image-agent-cli CMD`：自定义容器引擎命令（如 `"sudo -n podman"`）。
- `--image-agent-keep-output`：保留 agent 生成的临时输出便于调试。
- `--image-agent-keep-rootfs`：保留 agent 导出的 rootfs 便于检查。
- `--allow-gpu-libs`：允许将 GPU/DRM 相关库（如 libdrm/libnvidia 等）打包，不再过滤。
- `--strict-validate`：链接器验证失败时中止构建。
- `--copy-dir SRC[:DEST]`：递归复制宿主目录到 bundle payload 中。如果省略 DEST，文件将按源目录结构放置在 `payload/` 下。
- `--set-env KEY=VALUE`：覆盖生成启动器中的环境变量（可重复）。
- `--run-mode MODE`：设置启动器的运行时执行模式（`Host`、`Bwrap` 或 `Chroot`）。默认为 `Host`。
- `--log-level`：日志级别（`error`、`warn`、`info`、`debug`、`trace`）。

## 宿主 + 镜像混合示例

```bash
sidebundle create \
  --name demo \
  --from-host '/opt/tools/foo::trace="/opt/tools/foo --warmup"' \
  --from-image 'podman://registry/my/api:stable::/usr/bin/api-server' \
  --trace-backend combined \
  --image-trace-backend agent
```

输出包含 `foo` 与 `api-server` 的启动器以及合并的 payload。

## 容器内跟踪
选择 `--image-trace-backend agent` 时，CLI 会把 `sidebundle` 复制到容器并执行
`sidebundle agent trace`，使用 JSON 规范描述每个命令，输出的报告将合并进依赖闭包。
如需在容器内使用组合跟踪，可指定 `--image-trace-backend agent-combined`。
调试时可结合 `--image-agent-keep-output` 或 `--image-agent-keep-rootfs`。

手动调试示例：

```bash
sidebundle agent trace \
  --rootfs /payload \
  --spec /tmp/spec.json \
  --output /tmp/trace \
  --trace-backend ptrace
```

## 运行 bundle
每个入口在 `bin/` 下有包装脚本，流程如下：

1. 解析 `BUNDLE_ROOT`。
2. 设置 `SIDEBUNDLE_ROOT`。
3. 确认 linker 与二进制存在。
4. 导出包含计划中所有目录的 `LD_LIBRARY_PATH`。
5. 对静态二进制直接执行，对动态二进制通过 runtime linker 启动。

将整个 bundle 目录拷贝到目标机器，执行 `bin/<entry>` 即可。

## 验证与调试
- 验证：构建完成后会运行 `BundleValidator`。配合 `--strict-validate` 在缺失依赖时失败。
- 日志：使用 `--log-level debug` 查看解析/合并/验证细节。
- manifest：`manifest.lock` 包含复制的依赖 (`files`) 与运行时跟踪的文件 (`traced_files`) 及其摘要。
- 跟踪产物：运行时文件保存在 `resources/traced`，保留原始逻辑路径，便于审计。

## 特殊场景与提示

- **fanotify 跟踪“卡住”**：fanotify 监听文件事件，如果命令卡死，多数是其他进程占用了相关文件/目录或有额外监视导致未能退出。检查是否有其他文件监控程序，必要时换用更简单的命令或切换到 `ptrace/combined`。
- **高风险资产过滤**：sidebundle 会过滤部分 GPU/DRM 相关库（如 `libdrm`、`libnvidia*`），以避免打包宿主驱动。若你的场景需要这些库（例如 ffmpeg 使用 DRM/VAAPI/NVENC），请加 `--allow-gpu-libs` 放行，并确保目标环境有匹配的设备/驱动。
- **trace 参数在宿主/镜像中的区别**：
  - 宿主：`--trace-backend` 支持 `off|auto|ptrace|fanotify|combined`。
  - 镜像：`--image-trace-backend` 支持上述选项，另外有 `agent|agent-combined` 在容器内运行跟踪。Agent 模式需 Docker/Podman 提供 `SYS_PTRACE`/`SYS_ADMIN` 和 `seccomp=unconfined`。
  JVM 这类通过 `dlopen` 加载库的场景，推荐 `combined`/`agent-combined` 捕获完整依赖。
- **权限矩阵**：见 `docs/permissions.md`，列出不同跟踪后端与运行模式的权限/能力要求。

## sidebundle 如何收集依赖

```
来源：
  - 宿主入口 (--from-host)   -> chroot/host 解析 -> ELF/shebang 解析 -> 静态依赖
  - 镜像入口 (--from-image)  -> 导出 rootfs 或在容器内运行 agent

静态阶段：
  解析 ELF (DT_NEEDED) / shebang -> 解析解释器 -> 复制二进制/库到 payload

运行时跟踪（可选）：
  ptrace/fanotify/agent(-combined) -> 收集 exec/open 路径 -> 提升 ELF/资源到闭包

打包：
  文件去重 -> 链接到 payload/data -> 写 manifest.lock -> 生成启动器
```

## 开发与测试

```bash
cargo fmt
cargo clippy --all-targets
cargo test --workspace
```

欢迎提交 issue 或 PR 来覆盖新的闭包场景。
