# 特殊场景处理备忘

这里列出 sidebundle 为覆盖特定场景而加入的非直觉性处理，便于查阅和回溯设计背景。

## 多调用（二进制自检）兼容
- 目的：busybox / uutils coreutils 等多调用 ELF 会校验 `argv0` 与 `/proc/self/exe` 指向的可执行文件是否一致，否则拒绝执行。
- 打包侧：优先将 data 中的文件以硬链接形式放到 `payload/`，避免 `/proc/self/exe` 落在哈希路径上。（`sidebundle-packager/src/lib.rs:link_or_copy`）
- 运行侧：
  - Host 模式依旧显式调用打包的 ld-linux，保持 ABI 保证；多调用需要使用下述隔离模式。
  - bwrap/chroot 模式直接 exec 入口（不显式 ld-linux），根指向 payload，PT_INTERP 使用打包的 ld-linux，`/proc/self/exe` 与入口一致，校验可通过。（`sidebundle-launcher/src/main.rs:exec_bwrap/exec_chroot`）

## Node shebang 保持符号链接
- 目的：node 解析 shebang 时若跟随符号链接，部分脚本会出问题。
- 处理：为 node 解释器注入 `NODE_OPTIONS=--preserve-symlinks-main --preserve-symlinks`。（`sidebundle-packager/src/launcher.rs:inject_script_metadata`）
  - 额外：对 node shebang 脚本注入 `NODE_PATH=/usr/share/nodejs`，以兼容 Debian/Ubuntu 的全局 JS 模块布局（如 npm 运行时依赖 semver 等）。

## PATH/LD_LIBRARY_PATH 映射与 JVM 兼容
- 目的：Host 模式下把打包路径放到前面，避免回落宿主；JVM/dlopen 常需要兄弟 lib 目录。
- 处理：映射绝对 PATH 条目到 bundle，追加每个 bin 目录的 `lib`/`lib/server` 到 `LD_LIBRARY_PATH`，并将绝对 `JAVA_HOME`/`GOROOT` 映射到 payload。（`sidebundle-launcher/src/main.rs:build_env_block`）

## 系统配置文件兜底
- 目的：镜像内可能缺失或为空的基础配置。
- 处理：对空的 `payload/etc/resolv.conf` 回退宿主副本；缺失的 `/etc/passwd`、`/etc/group`、`/etc/nsswitch.conf`、`/etc/hosts` 等尝试从宿主复制。（`sidebundle-packager/src/lib.rs:is_empty_resolv_conf`, `collect_host_system_assets`）

## 运行时别名与设备节点
- 目的：脚本常用的解释器名或设备节点在最小化 bundle 中缺失。
- 处理：
  - 创建常见字符设备节点 `/dev/null`、`/dev/tty`、`/dev/zero`、`/dev/urandom`（若无法 mknod 则写空文件）。`sidebundle-packager/src/lib.rs:ensure_device_nodes`
  - 常见解释器别名（如 `python3` → `python3.10`）按存在性写入符号链接。`sidebundle-packager/src/lib.rs:ensure_aliases`

## 数据目录镜像
- 目的：在 chroot 模式下 `/data` 可读/可写且避免重复拷贝。
- 处理：优先将 `bundle_root/data` bind-mount 到 `payload/data`，无权限时退回硬链接/复制。`sidebundle-launcher/src/main.rs:ensure_payload_data`

## 自解压 shim（分发便利性）
- 目的：减少分发/安装步骤，提供单文件自解压包装。
- 处理：`--emit-shim` 时，打包阶段将 bundle 打成压缩 tar，附加到 shim stub，生成 `shims/<entry>` 可执行，运行时解压到缓存目录并调用 launcher。（`sidebundle-packager/src/shim.rs`, `sidebundle-shim` crate）

## GPU/DRM 依赖过滤
- 目的：避免误把宿主 GPU/DRM 相关库打包到可迁移 bundle 中，导致设备耦合或法律风险。
- 默认行为：过滤常见前缀如 `libdrm`、`libnvidia*`、`libgl*`、`libvulkan`、`libcuda` 等，不写入闭包。（过滤表见 `sidebundle-closure/src/lib.rs:GPU_LIB_PREFIXES`）
- 解除过滤：如确实需要这些库（NVENC/VAAPI/DRM 等），构建时加 `--allow-gpu-libs`，CLI 会放行相关依赖，前提是目标环境具备匹配设备/驱动。

## 常见 FAQ 指引
- 多调用二进制在 Host 模式报 “Requested utility …”：改用 bwrap/chroot 或非多调用版本。
- Java/Go 运行时绝对路径丢失：确认 `JAVA_HOME`/`GOROOT` 映射逻辑（见上）。
- 网络解析失败：检查 `payload/etc/resolv.conf` 是否为空，或宿主是否提供兜底。

## TODO：语言运行时资源自动收集
- 背景：Python/Node/Java 等语言层资源（stdlib、JS 内置、JRE modules/security 等）目前需用户通过 `::trace` 或 `--copy-dir` 明确引入；未采集会在运行时缺 `encodings`、内置 JS 等。
- 方向：提供可选的语言感知运行时探测（非交互、可配置），自动读取 `sys.path`/Node 内置列表/JRE modules 等并收集对应资源，降低打包踩坑概率，同时允许禁用以保证安全可控。
