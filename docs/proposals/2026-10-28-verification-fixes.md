# sidebundle 修复提案（基于 2026-10-28 软件池检验）

基准是一次黑盒检验，不是逐行处方。实现前对照了当前代码（HEAD 当时为 `176cf6f`，v0.3.0）。下面保留检验分类标签，并标明哪些假设被代码核对推翻。

## 基准与反馈来源

- 矩阵：检验表 `matrix.md`（2026-10-28，打包器 `sidebundle-x86_64-musl` v0.3.0）
- 中期报告：`interim-report.md`（失败聚类）
- 个案笔记：curl-host、tesseract、imagemagick、ffmpeg-debian、psql、semgrep
- 更早的代码分析（P0/P1 清单）只作线索；与现行代码冲突的地方以下面「代码核对」为准

验收目标机：host（debian 13）/ centos:7 / ubuntu 18.04。带 `*` 的 PASS 含手工补丁，不算产品可用。

## 代码核对（相对笔记里的假设）

| 假设 | 核对结果 |
|---|---|
| launcher 在 `metadata.env` 非空时整表替换调用方环境（P0-1） | **成立**。`build_env_block` 原先在有 metadata 时不用 `env::vars()`。`--set-env` 与 agent 捕获的环境都写进同一个 `env` map。 |
| launcher 不展开 `${SIDEBUNDLE_ROOT}`，CA 路径写死 `/b` | **成立**。`SIDEBUNDLE_ROOT` 只被写入环境，不参与替换。 |
| 发布物默认是 gnu 动态 launcher（P0-3） | **部分不成立**。`.github/workflows/release.yml` 与 tag 上的 `ci.yml` 已经用 `*-unknown-linux-musl`。失败的是 **gnu 开发构建**：`sidebundle-packager/build.rs` 按宿主 `TARGET` 嵌 launcher，动态链接构建机 glibc，centos:7 / ubuntu 18.04 上 `GLIBC_* not found`（矩阵行 `true (gnu 开发构建)`）。 |
| closure 没有递归第二层 NEEDED（liblqr → libglib） | **不成立为「没有循环」**。`build_binary_plan` 会把 linker 解析到的库放进队列再跑 `ld.so --list`。实际失败模式是：`ld.so --list` 缺库时 **非 0 退出**，旧代码在读 stdout 之前就当成命令失败；私有目录（`pulseaudio/libpulsecommon-*.so`）因此 PACK_FAIL（矩阵：ffmpeg debian / **MISSING_SO**）。dlopen 的 ImageMagick `modules-*/coders` 本来就不在 DT_NEEDED 里，trace `-version` 也不会打开它们。 |
| `/usr/bin/convert` alternatives、psql `pg_wrapper`（**IMAGE_PATH**） | **成立**。`canonicalize` 使用 `fs::canonicalize`，绝对符号链接会离开镜像根，失败后退回未解析路径。shebang 的 Perl wrapper 被当成脚本打包，没有指向 `/usr/lib/postgresql/<ver>/bin/psql`。 |
| semgrep 子进程带着 musl `PT_INTERP`，绕开 launcher（**CHILD_EXEC**） | **成立，且本期不在 host 模式里修**。launcher 只 `exec` 入口；子进程 `execve` 由内核按子 ELF 的 `PT_INTERP` 加载。bwrap/chroot 把 payload 挂到 `/` 后，这条路径才可能落到包内的 ld-musl。chroot 要补 `/proc`、`/dev`、可写 tmp、工作目录，超出本 PR。 |

## 优先级

### P0-A 环境合并（矩阵：tesseract、curl-setenv、P0-1、PATH_ENV）

**现象**：`metadata.env` 非空时调用方的 `TESSDATA_PREFIX`、`HTTPS_PROXY`、`PATH` 全部丢失。

**实现**：

1. `--set-env` 写入 `RuntimeMetadata.env_overrides`，始终覆盖同名变量。
2. 调用方环境为底。
3. `env_defaults`（产品注入的可移植前缀）只填补缺失键，并高于 agent 捕获值。
4. 历史字段 `env`（agent 整表）只填补仍然缺失的键，不再替换。

旧 bundle 没有 `env_overrides` 时，原来的 `env` 也按「默认值」合并，从而不再清空调用方环境。

### P0-B 可移植数据前缀（矩阵：curl-host、curl-setenv、tesseract、PATH_ENV / MISSING_DATA）

**现象**：证书和 tessdata 在 payload 里（或本可以放进去），程序仍读宿主绝对路径；`--set-env SSL_CERT_FILE=/b/payload/...` 绑死挂载点，`${SIDEBUNDLE_ROOT}` 不展开。

**实现**：

- 打包：入口是 curl，或闭包里有 `libssl` / `libcrypto` / `libcurl` 时，从该 origin 根复制常见 CA 文件，并把 `SSL_CERT_FILE` / `CURL_CA_BUNDLE` / `REQUESTS_CA_BUNDLE` 写成 `${SIDEBUNDLE_ROOT}/payload/...`。
- 打包：入口是 tesseract 时，复制含 `*.traineddata` 的 tessdata 目录（优先含 `eng.traineddata`），设置 `TESSDATA_PREFIX`。
- 启动：展开 `${SIDEBUNDLE_ROOT}` 与相对 bundle 根的路径；若绝对路径在 payload 中存在对应文件，改写到 payload；调用方未设置且宿主路径不存在时，按 payload 里的已知布局补上。调用方或 `--set-env` 已给出的值不改写。

### P0-C musl / 静态 launcher（矩阵：true gnu、LAUNCH_FAIL、P0-3）

**现象**：gnu 动态 launcher 在 centos:7 / ubuntu 18.04 无法启动。发布物本身已是 musl。

**实现**：

- `build.rs` 在 musl target 与 `rust-lld` 可用时，把嵌入的 launcher 和 shim 编成对应 musl target（可用 `SIDEBUNDLE_LAUNCHER_TARGET` 覆盖）。
- 嵌入的 ELF 若仍有 `PT_INTERP`，编译期 `cargo:warning`，打包时再 `warn`。
- CI 的 lint/test job 安装 `x86_64-unknown-linux-musl`，避免「可迁移」默认路径悄悄嵌 gnu launcher。
- `docs/install.md`、`docs/faq.md` 写明 gnu 开发构建不可迁移。

### P1-A closure：传递依赖、私有库目录、ImageMagick coder（矩阵：ffmpeg debian MISSING_SO；imagemagick IM6 MISSING_SO + MISSING_DATA）

**实现**：

- `ld.so --list` 非 0 退出但 stdout 里已有解析结果时，保留已解析项和 `=> not found` 名单。
- 对当前 ELF 的 `DT_NEEDED`，在主体目录、搜索路径，以及主体目录的**一层**子目录（`pulseaudio/`）里按文件名查找。找到才继续，直接依赖仍缺失则失败。
- 队列本身继续递归，因此 liblqr 的 `libglib` 会在处理 liblqr 时补上，而不是在父 ELF 的一次 `ld.so` 输出不完整时整包失败。
- 闭包里出现 ImageMagick / `libMagick*` 时，在 origin 根下收集 `ImageMagick…/modules-*/coders|filters/*.so`，收入 payload 并尽量解析其 NEEDED。单个 coder 解析失败只警告，不让整包失败。

### P1-B 入口解引用（矩阵：imagemagick `/usr/bin/convert` IMAGE_PATH；psql pg_wrapper IMAGE_PATH）

**实现**：

- 有 root 时不再用 `fs::canonicalize` 追绝对符号链接。在 root 内逐级 `readlink`，`/usr/bin/convert` → `/etc/alternatives/convert` → `/usr/bin/convert-im6.q16` 会落到镜像内的真实文件。
- 链在 root 内断开时，错误信息点名 alternatives，并提示 ImageMagick 常见真实路径 `/usr/bin/convert-im6.q16`。
- 内容像 `pg_wrapper` / `PgCommon` 的脚本：若 `usr/lib/postgresql/*/bin/<同名>` 恰好有一个 ELF，改用它并警告；多个则报错列出路径；零个则报错提示 `/usr/lib/postgresql/<version>/bin/<tool>`。

### P1-C CHILD_EXEC（矩阵：semgrep）— 本期不做

子进程 `semgrep-core` 的 `PT_INTERP` 是 `/lib/ld-musl-x86_64.so.1`。host 模式下内核加载子 ELF，launcher 插不上手。把 `PT_INTERP` 改成打包时未知的 bundle 绝对路径也不可重定位。

可行的后续方向（单独 PR）：

1. host 模式的 exec 拦截（例如由 bundle ld.so 加载的 `LD_PRELOAD` 把子 `execve` 重新交给 bundle linker），或
2. 把 chroot/bwrap 补全到检验笔记里的程度：`/proc`、真实 `/dev`、可写 tmp、调用方工作目录、`$ORIGIN` RUNPATH、busybox applet。bwrap 已有 `--proc` 和部分 `/dev`，但仍 `--unshare-all`（断网）且不绑定 cwd。

在此之前，semgrep 行保持 FAIL；`semgrep-chroot-lp` 仍是 PASS*（手工补丁）。

### P2（记录，不实现）

git templates/libexec、Python stdlib、semgrep site-packages 仍依赖 `--copy-dir`。检验已证明手工 copy-dir 可以过。不删除该开关。

## 验收（相对本检验，产品路径、不靠手工改 JSON / 固定 `/b`）

用 musl 发布物（或本变更之后嵌入 musl launcher 的构建）在 centos:7 与 ubuntu 18.04 上：

| 矩阵行 | 期望 |
|---|---|
| curl-host | PASS。自动带上 CA，并设置可展开的 `SSL_CERT_FILE` / `CURL_CA_BUNDLE`。不需要 docker `-e`，也不写死 `/b`。 |
| curl-setenv | PASS，且 `-e HTTPS_PROXY=...` 不再被 metadata 整表丢掉。`${SIDEBUNDLE_ROOT}` 会展开。 |
| tesseract | `--list-langs` 含 eng，无需手改 launcher JSON。调用方已设置的 `TESSDATA_PREFIX` 仍优先。 |
| true (gnu 开发构建) | 安装 musl target 后，嵌入 launcher 无 `PT_INTERP`，centos:7 不再因 launcher 的 glibc 失败。未安装 musl target 时构建会警告。发布物本来就是 musl，保持 PASS。 |
| ffmpeg debian | 私有 `pulseaudio/` 不再使 PACK 因 `libpulsecommon` 失败。单测覆盖该查找。运行时 wav→mp3 仍建议用已 PASS 的 jrottenberg 镜像对照。 |
| imagemagick `/usr/bin/convert` | alternatives 链在镜像根内可解析时 PACK 成功，入口名仍是 `convert`。链断开时错误信息给出 `convert-im6.q16`。 |
| imagemagick convert-im6.q16 | coder 模块与传递 NEEDED 进入闭包。是否在 centos 上完成 png→jpg 取决于模块是否被启发式扫到以及是否混链；单测锁定「coders/filters 被收集」和「私有目录 / 传递 NEEDED 文件名查找」。 |
| psql pg_wrapper | 唯一的 `…/bin/psql` ELF 会被自动选中（矩阵里真实 ELF 行已经全绿）。多个版本时报出路径列表。 |
| semgrep | **仍 FAIL**（CHILD_EXEC）。不把 PASS* 的手工 chroot 当成产品行为。 |

单元测试：launcher 的环境合并与 `${SIDEBUNDLE_ROOT}` / 相对路径展开；closure 的 alternatives、pg_wrapper、pulse 私有库、ImageMagick 模块目录；CLI 的 CA / tessdata 注入。

## 明确不做

- 完整 32-case 检验脚本落盘
- Mac/Mach-O、daemon、GPU
- 删掉 `--copy-dir`
- 在本 PR 里实现 CHILD_EXEC 或改写 payload 内所有 ELF 的 `PT_INTERP`
- 把 agent 捕获的整份环境从 JSON 里删掉（仍作为最低优先级默认值；令牌过滤留给后续）
