# sidebundle permissions at a glance

This doc explains the minimum capabilities/privileges needed on the build machine for each trace backend, and on the target machine for each run mode. Linux only.

## Build-time: trace backend requirements

| Backend | Scope | Required capability/privilege | Notes |
| --- | --- | --- | --- |
| `off` | No runtime tracing | None | Static ELF/shebang analysis only. |
| `ptrace` | Intercept `execve/open*` | - Child processes you own: usually no extra caps, but `kernel.yama.ptrace_scope` must allow (0/1).<br>- Arbitrary/deeper: needs `CAP_SYS_PTRACE`. | Without caps you’ll see `ptrace not permitted`. |
| `fanotify` | Filesystem open/exec watch | Needs `CAP_SYS_ADMIN` (uses `FAN_MARK_FILESYSTEM` on the mount). | Cannot scope to a single directory; use a separate mount namespace to narrow impact. |
| `combined` | ptrace + fanotify | Needs both `CAP_SYS_PTRACE` and `CAP_SYS_ADMIN`, plus fanotify allowed. | Default `auto` on Linux effectively means this. |
| `agent` / `agent-combined` (images) | Runs sidebundle agent inside container | Container engine must allow:<br>- `CAP_SYS_PTRACE` (always);<br>- `CAP_SYS_ADMIN` if fanotify/combined inside the container;<br>- `seccomp=unconfined` or rules allowing ptrace/fanotify syscalls;<br>- `--security-opt apparmor=unconfined` if AppArmor blocks;<br>Also needs Docker/Podman pull permission. | `agent-combined` needs both caps. |

### Tips to minimize privileges
- **ptrace only**: grant `CAP_SYS_PTRACE` to `sidebundle-cli` (`sudo setcap cap_sys_ptrace+ep ./sidebundle-cli`) or run inside an unprivileged user namespace.
- **need fanotify/combined**: run inside an isolated user+mount namespace (e.g., `unshare -m -U -r`/`bwrap`), grant `CAP_SYS_ADMIN` inside that namespace, and bind-mount only the source tree to limit exposure.

## Runtime: run-mode requirements (target machine)

| run-mode | Required deps/privileges | Notes |
| --- | --- | --- |
| `Host` (default) | None beyond reading the bundle. | Executes directly on host FS with packaged linker/libs. |
| `Bwrap` | - `bwrap` installed;<br>- Unprivileged user namespaces enabled (common on modern distros);<br>- If disabled, need setuid bwrap or run as root. | Launcher calls `bwrap --unshare-all --bind <payload> /` to build a minimal root. Smaller host visibility. |
| `Chroot` | `CAP_SYS_CHROOT` (typically root). | Launcher chroots into bundle payload; strongest isolation, highest requirement. |

### Other runtime notes
- DNS/hosts: `bwrap` mode bind-mounts `/etc/resolv.conf` and `/etc/hosts` read-only to keep name resolution working.
- GPU/DRM devices: `bwrap`/`chroot` only bind a few pseudo devices (null/zero/tty/urandom). For GPU access, extend the launcher or run in `Host` mode.

## Practical checks
- Inspect ptrace scope: `cat /proc/sys/kernel/yama/ptrace_scope` (0/1 allows non-root to trace children).
- If `auto/combined` fails on host, force `--trace-backend ptrace` to reduce privilege needs.
- If you need fanotify but avoid global root: create an isolated namespace with `bwrap`/`unshare`, grant caps inside, then run `sidebundle-cli` there.***
