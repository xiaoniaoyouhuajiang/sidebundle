# sidebundle 权限需求速览

本文用于说明在**打包阶段**使用不同跟踪后端（trace backend）以及在**运行阶段**使用不同运行模式（run-mode）时，对宿主机/目标机需要具备的最低权限和依赖。默认只讨论 Linux（项目仅支持）。

## 打包阶段：trace backend 权限

| 后端 | 作用范围 | 需要的能力/权限 | 备注 |
| --- | --- | --- | --- |
| `off` | 不做运行时跟踪 | 无 | 仅静态解析 ELF/shebang。 |
| `ptrace` | ptrace 拦截 `execve/open*` | - 跟踪自身子进程：通常无需额外能力，但 `kernel.yama.ptrace_scope` 必须允许（0/1）。<br>- 跟踪任意进程/更深：需要 `CAP_SYS_PTRACE`。 | 若缺权限会报 `ptrace not permitted`。 |
| `fanotify` | 监听文件系统 open/exec | 需要 `CAP_SYS_ADMIN`（对挂载点做 `FAN_MARK_FILESYSTEM`）。 | 无法仅给单目录授权；可用隔离的 mount namespace 缩小范围。 |
| `combined` | 同时跑 ptrace + fanotify | 需要 `CAP_SYS_PTRACE` + `CAP_SYS_ADMIN`，并允许 fanotify 标记。 | Linux 默认 `auto` 会选择此组合。 |
| `agent` / `agent-combined`（镜像） | 在容器内跑 sidebundle agent | 容器运行时需允许：<br>- `CAP_SYS_PTRACE`（必需）；<br>- `CAP_SYS_ADMIN`（若在容器内使用 fanotify/combined）；<br>- `seccomp=unconfined` 或放行 fanotify/ptrace 相关 syscalls；<br>- `--security-opt apparmor=unconfined`（若宿主启用 AppArmor）；<br>另需 Docker/Podman 具备拉取镜像权限。 | 对镜像来说 `--image-trace-backend` 可单独设定；`agent-combined` 需要同上两种能力。 |

### 权限收缩建议
- **仅需要 ptrace**：给 `sidebundle-cli` 授权 `CAP_SYS_PTRACE`（`sudo setcap cap_sys_ptrace+ep ./sidebundle-cli`），无需 root 全权；或在 user namespace 内授予。
- **必须 fanotify/combined**：在隔离的 user+mount namespace 里运行（如 `unshare -m -U -r`/`bwrap`），在 namespace 内赋予 `CAP_SYS_ADMIN`，把目标源码目录 bind mount 进去，避免对宿主全局提权。

## 运行阶段：run-mode 权限

| run-mode | 需要的依赖/权限（目标机） | 说明 |
| --- | --- | --- |
| `Host`（默认） | 无额外权限；能读 bundle 目录即可。 | 直接在宿主文件系统运行，使用打包时记录的 linker 与库路径。 |
| `Bwrap` | - 目标机已安装 `bwrap`（bubblewrap）；<br>- 需要能创建 unprivileged user namespace（多数发行版默认开启）；<br>- 若禁用 unpriv userns，则需要 setuid bwrap 或以 root 运行。 | launcher 调用 `bwrap --unshare-all --bind <payload> /` 创建最小 rootfs。对宿主文件系统可见性更小。 |
| `Chroot` | 需要 `CAP_SYS_CHROOT`（通常 root）。 | launcher `chroot` 到 bundle 的 payload，再执行入口；隔离强，但要求最高。 |

### 运行阶段的其它注意事项
- 动态二进制依赖的 `/etc/resolv.conf`/`/etc/hosts`：在 `bwrap` 模式下会只读绑定宿主文件，保证解析正常。
- GPU/DRM 设备：`bwrap`/`chroot` 默认只绑定少量伪设备（null/zero/tty/urandom）。需要使用 GPU 时需手工扩展 launcher 或在宿主运行 `Host` 模式。

## 实操提示
- 检查 ptrace scope：`cat /proc/sys/kernel/yama/ptrace_scope`（0/1 才允许非 root 跟踪子进程）。
- 如果 `auto/combined` 在宿主失败，可以强制 `--trace-backend ptrace`，减少权限需求。
- 若需要 fanotify 但不想全局给 root：用 `bwrap`/`unshare` 创建隔离 namespace，内里赋能后再运行 `sidebundle-cli`。
