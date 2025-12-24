# bwrap (bubblewrap) run mode & embedded-bwrap

`--run-mode bwrap` runs the bundle entry inside a bubblewrap sandbox where the bundle `payload/` is mapped as `/`. This is usually the most reproducible way to validate relocatability.

## Requirements

To use bwrap, you typically need one of:
- **Unprivileged user namespaces** enabled on the kernel, or
- A **setuid** system `bwrap` (rare; has security implications).

If you see:

```
bwrap: Creating new namespace failed, likely because the kernel does not support user namespaces. bwrap must be installed setuid on such systems.
```

your environment cannot use userns (or is restricted by a container/sandbox). Use another host, or fall back to `--run-mode host`/`chroot`.

## Containers & permissions

When running inside containers, bwrap also needs enough privileges to create and configure namespaces. If you see:

```
bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted
```

then the process lacks `CAP_NET_ADMIN` in the new network namespace (common in rootless Docker/Podman or non-root container users). Fixes:

- Run the container as root with sufficient privileges (`--privileged` on rootful Docker).
- Avoid bwrap for that environment and use `--run-mode host`.

## embedded-bwrap binaries

GitHub Releases ship two musl binaries:
- `sidebundle-*-musl`: no embedded bwrap (requires system `bwrap` for `--run-mode bwrap`)
- `sidebundle-*-musl-embedded-bwrap`: embeds a static bwrap (no system bwrap needed, but userns is still required)

The embedded bwrap is extracted on first use to:
- `~/.cache/sidebundle/bwrap/<arch>/<sha256>/bwrap`

## Override bwrap selection

Priority order:
1. `SIDEBUNDLE_BWRAP=/abs/path/to/bwrap`
2. system `bwrap` from `PATH` / common locations
3. extracted embedded bwrap (embedded builds only)
