# bwrap（bubblewrap）运行模式与 embedded-bwrap

sidebundle 的 `--run-mode bwrap` 会使用 bubblewrap 将 bundle 的 `payload/` 映射为进程的 `/`，从而更严格、更可复现地运行入口（更容易暴露缺依赖/路径不一致问题）。

## 系统要求

要使用 bwrap，通常需要满足其一：
- 内核支持并启用 **unprivileged user namespace**（常见于多数桌面/服务器发行版的默认配置）；或
- 系统的 `bwrap` 以 **setuid** 方式安装（较少见，且有额外安全含义）。

如果你看到类似报错：

```
bwrap: Creating new namespace failed, likely because the kernel does not support user namespaces. bwrap must be installed setuid on such systems.
```

说明当前环境无法使用 unprivileged userns（或被容器/沙箱限制），需要更换运行环境或改用 `--run-mode host`/`chroot`。

## 容器内运行与权限

如果你在容器内运行 bwrap，额外需要有足够的权限来创建和配置 namespace。若出现：

```
bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted
```

说明新建 netns 时缺少 `CAP_NET_ADMIN`（rootless Docker/Podman 或以非 root 用户运行时常见）。可选方案：

- 使用 root 用户、rootful Docker，并赋予足够权限（如 `--privileged`）。
- 在该环境中改用 `--run-mode host`。

## embedded-bwrap：无需系统安装 bwrap

GitHub Releases 同时提供两套 musl 二进制：
- `sidebundle-*-musl`：不内嵌 bwrap（需要系统已安装 `bwrap` 才能用 `--run-mode bwrap`）。
- `sidebundle-*-musl-embedded-bwrap`：内嵌一个静态 bwrap，目标机无需预装 bwrap（但仍需要 userns 能用）。

embedded-bwrap 版本在第一次需要 bwrap 时会将内嵌的 bwrap 解压到缓存目录：
- `~/.cache/sidebundle/bwrap/<arch>/<sha256>/bwrap`

随后重复使用缓存命中的 bwrap。

## bwrap 的查找与覆盖

launcher 会按以下优先级选择 bwrap：
1. 显式覆盖：环境变量 `SIDEBUNDLE_BWRAP=/absolute/path/to/bwrap`
2. 系统 bwrap：`PATH` 与常见路径（如 `/usr/bin/bwrap`）
3. embedded-bwrap：从内嵌数据解出到缓存并使用（仅 embedded 版本）

这对于排障很有用：你可以用 `SIDEBUNDLE_BWRAP` 强制指定某个 bwrap 版本，验证行为差异。

## 什么时候应该用 bwrap

- 迁移验证/CI：优先用 `--run-mode bwrap`（更严格，能更早发现缺文件）。
- 目标环境不支持 userns：改用 `--run-mode host`（但更容易被宿主兜底，迁移验证能力弱）。
