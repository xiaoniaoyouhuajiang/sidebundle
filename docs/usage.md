# CLI 使用指南（进阶）

本页面向日常使用 sidebundle 的“配方化”说明，覆盖：
- `create` 的输入规格（host/image）
- `::trace` 如何写、如何触发依赖
- `--run-mode` 与可迁移性的关系
- 常见 runtime（Python/Node/Java）打包要点

如果你只需要最短路径，请优先看 README 的 Quick start，然后再回到这里查细节。

## 术语

- **bundle**：sidebundle 输出的目录（例如 `target/bundles/<name>` 或 `--out-dir` 下）。
- **entry**：bundle 的入口（`bin/<entry>`），由 launcher 包装后可直接执行。
- **payload**：按运行时路径布局的文件树（`payload/...`）。
- **data**：去重存储（内容寻址），payload/alias 通过硬链接/复制引用它。
- **trace**：运行时跟踪阶段，用于捕获静态解析之外的依赖与资源文件。

## Bundle 布局（快速识别）

一个典型 bundle 目录结构如下（实际内容取决于输入与 trace）：

```
bundle-name/
  bin/                 # 启动器入口
  data/<sha256>        # 去重后的文件存储（payload/alias 通常会引用它）
  payload/...          # 按运行时路径放置的文件树（以 / 开头的绝对路径布局）
  resources/traced/... # 运行时跟踪捕获的文件（便于审计）
  manifest.lock        # 描述所有发布文件的 manifest
```

## `create`：从宿主机打包（`--from-host`）

规格：
- `--from-host PATH`
- `--from-host 'PATH::trace=<command>'`

其中：
- `PATH` 可以是 ELF、脚本（shebang）、或任意可执行文件。
- `::trace=<command>` 会在跟踪阶段执行，用于触发依赖加载。

示例：

```bash
sidebundle-cli create \
  --name demo \
  --from-host '/usr/bin/htop::trace="/usr/bin/htop -n 1"' \
  --out-dir target/bundles \
  --trace-backend combined \
  --run-mode bwrap
```

## `create`：从 OCI 镜像打包（`--from-image`）

规格：
- `--from-image '[backend://]IMAGE::/absolute/path'`
- `--from-image '[backend://]IMAGE::/absolute/path::trace=<command>'`

示例：

```bash
sidebundle-cli create \
  --name jq-alpine \
  --from-image 'docker://alpine:3.20::/usr/bin/jq::trace=--version' \
  --out-dir target/bundles \
  --image-trace-backend agent-combined \
  --run-mode bwrap
```

## 运行模式（`--run-mode`）

sidebundle 的 launcher 有多种运行模式：

- `host`：直接在宿主上运行（会显式使用 bundle 内 `ld-linux` 以保证 ABI），但更容易“被宿主环境兜底”，因此迁移验证能力较弱。
- `bwrap`：使用 bubblewrap 把 `/` 映射到 `payload/` 运行，能最大化暴露缺依赖/路径不一致问题（需要系统有 `bwrap`，且允许 userns；或使用 embedded-bwrap 版本，详见 `docs/bwrap.md`）。
- `chroot`：类似 `bwrap` 的根切换语义，通常需要更高权限或更受限的环境支持。

经验法则：
- 想要“可迁移性/可复现性”优先：`--run-mode bwrap`
- 想要“打包机上先跑通”优先：`--run-mode host`（但不要用它当作迁移验证）

## 跟踪后端（`--trace-backend`）

详见 `docs/tracing.md`。一般建议：
- 动态 ELF：`combined`（权限允许时）
- 纯静态 ELF：`off` 或 `auto`
- 语言 runtime/资源树：优先 `combined`，并配合 `::trace` 做更明确触发

## 常用参数速查

- `--out-dir DIR`：bundle 输出目录。
- `--target linux-x86_64|linux-aarch64`：目标平台。
- `--copy-dir SRC[:DEST]`：将宿主目录递归复制到 payload（用于语言资源树兜底）。
- `--set-env KEY=VALUE`：覆盖/注入 launcher 的环境变量（可重复）。
- `--allow-gpu-libs`：允许 GPU/DRM 相关库进入闭包。
- `--log-level info|debug|trace`：调试用。

## 典型配方

### Python：脚本（shebang）打包

Python 脚本常见问题是 stdlib/`encodings` 缺失，因此需要：
- 选择强 trace（`combined`）
- 用 `::trace` 触发 import
- 或直接 `--copy-dir` 把 stdlib 引入（依发行版路径不同）

示例（触发 import）：

```bash
sidebundle-cli create \
  --name hello-py \
  --from-host "/usr/bin/python3::trace=-c 'import encodings; import sys; sys.exit(0)'" \
  --out-dir target/bundles \
  --run-mode bwrap \
  --trace-backend combined
```

示例（复制 stdlib）：

```bash
sidebundle-cli create \
  --name python3 \
  --from-host "/usr/bin/python3::trace=-c 'import encodings; import sys; sys.exit(0)'" \
  --out-dir target/bundles \
  --run-mode bwrap \
  --copy-dir /usr/lib/python3.12
```

### Node：二进制打包

发行版的 node 有时依赖 `/usr/share/nodejs` 的全局模块树（尤其是 npm 相关脚本）。若你看到 `Cannot load externalized builtin` 或 `Cannot find module ...`，通常需要：
- 更强的 trace 触发（让模块加载路径被实际访问）
- 或 `--copy-dir /usr/share/nodejs`

示例（Ubuntu/Debian 常见）：

```bash
sidebundle-cli create \
  --name node \
  --from-host /usr/bin/node \
  --out-dir target/bundles \
  --run-mode bwrap \
  --trace-backend combined \
  --copy-dir /usr/share/nodejs
```

### Java：JRE 目录树 + 安全配置

Java 的 “可运行” 通常意味着不仅要带上 `java` ELF 和 `.so`，还要包含 JRE 的目录树与配置（如 `conf/security`）。

示例（以发行版 JRE 路径为准）：

```bash
sidebundle-cli create \
  --name java \
  --from-host "/usr/bin/java::trace=-version" \
  --out-dir target/bundles \
  --run-mode bwrap \
  --trace-backend combined \
  --copy-dir /usr/lib/jvm/java-17-openjdk-amd64 \
  --copy-dir /etc/java-17-openjdk/security:/usr/lib/jvm/java-17-openjdk-amd64/conf/security
```

## 迁移验证建议

最简单的迁移验证方式是：把 bundle 复制到“尽量干净”的同架构系统/容器中运行 entry。

建议优先用：
- `--run-mode bwrap`（更严格）
- 触发“真实工作负载”的 `::trace`（避免只跑 `--help` 却漏掉关键依赖）

## 更多说明

- 常见报错：`docs/faq.md`
- 特殊行为备忘：`docs/special_handling.md`
- 权限矩阵：`docs/permissions.md`
