# sidebundle

中文 | [English](README.md)

sidebundle 用于从动态链接的 ELF 可执行文件构建可迁移的离线包。CLI 可以从宿主机或 OCI 镜像（Docker/Podman）采集可执行文件，静态解析和运行时跟踪它们加载的文件，并生成可移植的目录结构（附启动器和 manifest）。

## 特性
- 打包来自宿主机或 OCI 镜像的可执行文件。
- 通过静态分析与运行时跟踪（ptrace/fanotify）解析依赖。
- 将多个入口合并为一个去重后的 payload。
- 支持 shebang 脚本，解析解释器、打包其依赖，并对 bash 脚本做常见调用的静态扫描。
- 生成设置 `SIDEBUNDLE_ROOT`、`LD_LIBRARY_PATH` 的启动脚本，可选通过原始 linker 启动。
- 在 `manifest.lock` 记录所有发布的文件，便于审计和可复现构建。

## 运行环境要求
- 仅支持 Linux 主机（当前目标 `linux-x86_64`）。运行时跟踪使用 ptrace/fanotify，若跟踪非自身进程需要 `CAP_SYS_PTRACE`（root 或等价权限）。
- Docker 或 Podman 用于拉取/展开 OCI 镜像，需允许 `--cap-add SYS_PTRACE` 和 `--security-opt seccomp=unconfined` 供跟踪代理使用。
- Rust 1.74+ 用于构建 CLI（更旧版本可能可用但未测试）。

## 构建与安装
```bash
# 本地构建
cargo build --release

# 安装到 ~/.cargo/bin
cargo install --path sidebundle-cli
```

CLI 提供上下文帮助：

```bash
sidebundle --help
sidebundle create --help
```

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
- `--target`：目标三元组，当前仅 `linux-x86_64`。
- `--out-dir DIR`：将 bundle 写入自定义根目录。
- `--trace-root DIR`：将宿主路径视为相对 DIR，便于使用 chroot/rootfs 内容。
- `--image-backend`：默认镜像提供者（`auto`、`docker`、`podman`）。
- `--image-agent-bin PATH`：指定挂载到容器的 `sidebundle` 二进制（默认当前 CLI）。
- `--image-agent-cli CMD`：自定义容器引擎命令（如 `"sudo -n podman"`）。
- `--image-agent-keep-output`：保留 agent 生成的临时输出便于调试。
- `--image-agent-keep-rootfs`：保留 agent 导出的 rootfs 便于检查。
- `--strict-validate`：链接器验证失败时中止构建。
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

## 开发与测试

```bash
cargo fmt
cargo clippy --all-targets
cargo test --workspace
```

欢迎提交 issue 或 PR 来覆盖新的闭包场景。
