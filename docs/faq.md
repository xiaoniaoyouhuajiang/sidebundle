# FAQ / 常见问题

本页聚焦“使用中最常见的报错与定位思路”。如果你遇到的是“某类特殊场景需要非直觉性处理”，也可以对照 `docs/special_handling.md`。

## 1) “Security violation: Requested utility … does not match executable name”

现象：执行 bundle 的 `bin/<entry>` 报错，提示请求的 utility 名称与实际执行路径不匹配。

常见原因：
- 目标是 **多调用（multi-call）** 程序（busybox/uutils coreutils 等），它会校验 `argv0`、`/proc/self/exe` 等一致性。
- Host 模式下显式调用 `ld-linux`，导致 `argv0` 变成 `ld-linux`，从而触发自检失败。

建议：
- 优先使用 `--run-mode bwrap` 或 `--run-mode chroot`（让程序直接作为入口被 exec）。
- 参考：`docs/special_handling.md` 的“多调用（二进制自检）兼容”。

## 2) Python：`ModuleNotFoundError: No module named 'encodings'`

原因：Python stdlib 未被打包进来（或路径不一致），`encodings` 是启动早期必需模块。

建议：
- 使用更强 trace，并用 `::trace` 明确触发 import：
  - 例：`/usr/bin/python3::trace=-c 'import encodings'`
- 或使用 `--copy-dir /usr/lib/pythonX.Y` 将 stdlib 引入 bundle（以发行版实际路径为准）。

## 3) Node/npm：`Cannot find module ...`

原因通常是 JS 资源树未完整包含，或运行时依赖“某条原始路径/拓扑”上的入口文件（而不仅仅是解析后的真实文件）。

建议：
- 提升 trace 的触发强度（让相关模块真的被加载/访问）。
- Ubuntu/Debian 常见布局：加 `--copy-dir /usr/share/nodejs`。
- 对 npm 这类复杂资源树：当前可能仍存在“捕获了真实文件但缺少入口拓扑”的问题，详见 `docs/special_handling.md` 的相关说明与 TODO。

## 4) Java：`Error loading java.security file`

原因：JRE 配置（如 `conf/security`）未在 bundle 内以期望路径出现。

建议：
- 复制 JRE 目录树（`--copy-dir /usr/lib/jvm/...`）
- 并将发行版的安全配置映射到 JRE 内期望路径：
  - 例如：`--copy-dir /etc/java-17-openjdk/security:/usr/lib/jvm/java-17-openjdk-*/conf/security`

## 5) bwrap 相关报错

### `bwrap: Can't find source path ... Permission denied`
原因：当前环境（例如某些 VM/容器）对 unprivileged user namespace 或挂载操作限制较多。

建议：
- 检查系统是否允许 unprivileged userns。
- 在 CI/容器里可能需要特权模式（例如 GitHub Actions 里用 privileged docker 跑 bwrap smoke）。

## 6) fanotify/ptrace 权限不足

现象：trace 无法生效，或表现为捕获不全。

建议：
- 对照 `docs/permissions.md` 先确认当前运行环境是否允许所选 backend。
- 在容器场景下优先考虑 image-agent 模式（把 trace 放到容器内做）。

## 7) `mknod EPERM ... writing stub file`

原因：打包阶段尝试创建 `/dev/null` 等字符设备节点，当前用户缺乏 `mknod` 权限。

影响：
- sidebundle 会写入“占位文件”兜底，但某些程序可能因此行为异常（例如读取随机数设备）。

建议：
- 尽量在允许创建设备节点的环境打包（或使用更强隔离/权限）。
- 或将运行模式切到更适配的隔离环境，避免依赖宿主 `/dev` 语义。

