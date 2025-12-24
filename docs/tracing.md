# 跟踪后端（trace backend）

sidebundle 的依赖收集分为两层：
1. **静态解析**：解析 ELF 的 `DT_NEEDED`、shebang 的解释器等。
2. **运行时跟踪（可选但推荐）**：在执行指定命令时捕获额外依赖（例如 `dlopen` 动态加载的 `.so`、运行时读取的配置/资源文件）。

本页解释 `--trace-backend`/`--image-trace-backend` 的取值、适用场景与限制。权限要求请结合 `docs/permissions.md` 一起看。

## 后端类型与选择建议

### `off`
- 只做静态解析，不做运行时跟踪。
- 适用：纯静态 ELF、或你明确知道不会在运行时加载额外文件的程序。
- 风险：容易漏掉 `dlopen`、资源文件、解释器运行时依赖等。

### `ptrace`
- 通过 ptrace 机制跟踪子进程行为，用于捕获运行时依赖线索。
- 适用：需要捕获进程 exec 链、动态加载、以及一些“运行时才知道”的依赖。
- 代价：权限要求较高，且对某些受限环境（容器/CI）更敏感。

### `fanotify`
- 通过 fanotify 监听文件访问事件，补齐“运行时读到的文件”。
- 适用：语言运行时、JVM、需要读取大量配置/资源文件的软件。
- 代价：权限要求更高，且宿主/容器/内核配置差异较大。

### `combined`
- 同时启用 ptrace + fanotify。
- 适用：权限允许时最稳妥的兜底组合（尤其是“ELF + 运行时资源树”）。

### `auto`
- Linux 上倾向选择更强的组合；在不支持的系统上退化为 no-op。
- 适用：你希望“一条命令尽量跑通”，但也需要接受不同环境下能力不同的事实。

### 镜像输入的 `agent` / `agent-combined`
- 用于 `--from-image`：将 trace 放到容器内部执行，然后把结果回传给打包进程。
- 适用：镜像内的依赖/路径与宿主差异很大，或宿主缺少运行镜像二进制的条件时。

## 跟踪命令（`::trace=`）的语义

`--from-host PATH::trace=<command>`（或 `--from-image ...::/path::trace=<command>`）会在跟踪阶段执行 `<command>`。

建议：
- 用“尽可能短、可退出”的命令触发依赖加载（例如 `-version`、`--help`）。
- 语言运行时的资源收集常需要更强的触发（例如 Python：`-c 'import encodings'`）。

## 控制 trace 体积（Python/Node）

trace 结果仅包含运行时实际访问到的文件。如果包体积异常膨胀，通常是运行时扫描了超出预期的目录
（例如 pyenv 或用户级 site-packages）。Python/Node 启动时可能扫描大量包目录，建议：

- 使用最小化的虚拟环境（只安装需要的包）。
- 设置 `PYTHONNOUSERSITE=1`，跳过用户级 site-packages。
- 尽量使用更明确的启动方式（例如 `python -S` 或 `-c 'import ...'`）。
- 通过 `PYTHONPATH` 限制搜索路径。

## bwrap/chroot 与 trace 的关系（易踩坑）

- `--run-mode` 控制最终生成的 launcher 如何运行 bundle（Host/Bwrap/Chroot）。
- 但 **trace 阶段也可能在隔离环境中执行**（取决于具体实现与组合策略）。

经验建议：
- 需要最大可复现性（避免宿主污染）时：优先选择 `--run-mode bwrap` 并配合 `combined`（权限允许的前提下）。
- 如果 trace 阶段发现“在隔离中找不到宿主 `.so`”之类问题，通常意味着环境变量/挂载范围不足；这类问题往往需要更明确的资源引入策略（例如 `--copy-dir` 或更强的 trace 触发）。

## 常见误区

- “trace 过了就一定能跑”：不一定。trace 捕获的是“本次执行触发到的文件”。如果你触发不充分，或者运行时还有其他分支会加载不同资源，仍可能缺文件。
- “复制一个目录就够了”：目录树里常包含软链接、别名路径、以及运行时按“原始路径”查找的入口文件。详见 `docs/special_handling.md` 里关于 symlink/alias 的说明。

## 相关文档

- 权限矩阵：`docs/permissions.md`
- 特殊场景备忘：`docs/special_handling.md`
- 常见报错排查：`docs/faq.md`
