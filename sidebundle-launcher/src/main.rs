use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use sidebundle_core::{RunMode, RuntimeMetadata};
use std::collections::BTreeMap;
use std::env;
use std::ffi::{CString, OsStr};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

fn main() {
    if let Err(err) = run() {
        eprintln!("sidebundle launcher: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let exe_path = env::current_exe().context("failed to resolve launcher path")?;
    let launcher_dir = exe_path
        .parent()
        .ok_or_else(|| anyhow!("launcher missing parent directory"))?;
    let bundle_root = launcher_dir
        .parent()
        .ok_or_else(|| anyhow!("launcher missing bundle root"))?;

    let invoked = env::args_os()
        .next()
        .ok_or_else(|| anyhow!("missing argv0"))?;
    let entry_name = Path::new(&invoked)
        .file_name()
        .ok_or_else(|| anyhow!("invalid launcher invocation"))?
        .to_string_lossy()
        .into_owned();

    let config = load_config(bundle_root, &entry_name)?;
    match config {
        LauncherConfig::Binary {
            dynamic,
            binary,
            linker,
            library_paths,
            metadata,
            run_mode,
        } => {
            let payload_root = bundle_root.join("payload");
            let entry_host = bundle_root.join(&binary);
            let entry_mapped = map_bundle_path(bundle_root, &binary, run_mode);
            let _linker_mapped = linker
                .as_ref()
                .map(|rel| map_bundle_path(bundle_root, rel, run_mode));
            let argv = build_binary_argv(&entry_mapped)?;
            let env_block =
                build_env_block(bundle_root, run_mode, &library_paths, metadata.as_ref())?;
            match run_mode {
                RunMode::Host => {
                    if !dynamic {
                        exec_static(&entry_host, &argv, &env_block)?;
                        unreachable!();
                    }
                    let linker_host = linker
                        .as_ref()
                        .map(|rel| bundle_root.join(rel))
                        .ok_or_else(|| anyhow!("dynamic launcher missing linker path"))?;
                    exec_dynamic(&linker_host, &entry_host, &argv, &env_block)?;
                    unreachable!();
                }
                RunMode::Bwrap => {
                    exec_bwrap(bundle_root, &payload_root, &entry_mapped, &argv, &env_block)?;
                    unreachable!();
                }
                RunMode::Chroot => {
                    exec_chroot(bundle_root, &payload_root, &entry_mapped, &argv, &env_block)?;
                    unreachable!();
                }
            }
        }
        LauncherConfig::Script {
            dynamic,
            interpreter,
            script,
            args,
            linker,
            library_paths,
            metadata,
            run_mode,
        } => {
            let payload_root = bundle_root.join("payload");
            let interpreter_host = bundle_root.join(&interpreter);
            let interpreter_mapped = map_bundle_path(bundle_root, &interpreter, run_mode);
            let script_mapped = map_bundle_path(bundle_root, &script, run_mode);
            let _linker_mapped = linker
                .as_ref()
                .map(|rel| map_bundle_path(bundle_root, rel, run_mode));
            let argv = build_script_argv(&interpreter_mapped, &script_mapped, &args)?;
            let env_block =
                build_env_block(bundle_root, run_mode, &library_paths, metadata.as_ref())?;
            match run_mode {
                RunMode::Host => {
                    if !dynamic {
                        exec_static(&interpreter_host, &argv, &env_block)?;
                        unreachable!();
                    }
                    let linker_host = linker
                        .as_ref()
                        .map(|rel| bundle_root.join(rel))
                        .ok_or_else(|| anyhow!("dynamic launcher missing linker path"))?;
                    exec_dynamic(&linker_host, &interpreter_host, &argv, &env_block)?;
                    unreachable!();
                }
                RunMode::Bwrap => {
                    exec_bwrap(
                        bundle_root,
                        &payload_root,
                        &interpreter_mapped,
                        &argv,
                        &env_block,
                    )?;
                    unreachable!();
                }
                RunMode::Chroot => {
                    exec_chroot(
                        bundle_root,
                        &payload_root,
                        &interpreter_mapped,
                        &argv,
                        &env_block,
                    )?;
                    unreachable!();
                }
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LauncherConfig {
    Binary {
        dynamic: bool,
        binary: PathBuf,
        linker: Option<PathBuf>,
        library_paths: Vec<PathBuf>,
        metadata: Option<RuntimeMetadata>,
        #[serde(default = "default_run_mode")]
        run_mode: RunMode,
    },
    Script {
        dynamic: bool,
        interpreter: PathBuf,
        script: PathBuf,
        args: Vec<String>,
        linker: Option<PathBuf>,
        library_paths: Vec<PathBuf>,
        metadata: Option<RuntimeMetadata>,
        #[serde(default = "default_run_mode")]
        run_mode: RunMode,
    },
}

fn default_run_mode() -> RunMode {
    RunMode::Host
}

fn load_config(bundle_root: &Path, entry_name: &str) -> Result<LauncherConfig> {
    let path = bundle_root
        .join("launchers")
        .join(format!("{entry_name}.json"));
    let data = fs::read(&path)
        .with_context(|| format!("failed to read launcher config {}", path.display()))?;
    serde_json::from_slice(&data)
        .with_context(|| format!("invalid launcher config {}", path.display()))
}

fn build_binary_argv(entry: &Path) -> Result<Vec<CString>> {
    let mut argv = Vec::new();
    argv.push(os_to_cstring(entry.as_os_str())?);
    for arg in env::args_os().skip(1) {
        argv.push(os_to_cstring(&arg)?);
    }
    Ok(argv)
}

fn build_script_argv(interpreter: &Path, script: &Path, args: &[String]) -> Result<Vec<CString>> {
    let mut argv = Vec::new();
    argv.push(os_to_cstring(interpreter.as_os_str())?);
    for arg in args {
        argv.push(CString::new(arg.as_bytes()).map_err(|err| anyhow!("invalid arg: {err}"))?);
    }
    argv.push(os_to_cstring(script.as_os_str())?);
    for arg in env::args_os().skip(1) {
        argv.push(os_to_cstring(&arg)?);
    }
    Ok(argv)
}

fn build_env_block(
    bundle_root: &Path,
    run_mode: RunMode,
    library_paths: &[PathBuf],
    metadata: Option<&RuntimeMetadata>,
) -> Result<Vec<CString>> {
    let mut env_map: BTreeMap<String, String> = metadata
        .cloned()
        .map(|meta| meta.env)
        .unwrap_or_else(|| env::vars().collect());
    env_map.insert(
        "SIDEBUNDLE_ROOT".into(),
        bundle_root.to_string_lossy().into_owned(),
    );

    if let Some(java_home) = env_map.get("JAVA_HOME").cloned() {
        if java_home.starts_with('/') {
            let mut mapped = bundle_root.to_path_buf();
            mapped.push("payload");
            mapped.push(java_home.trim_start_matches('/'));
            env_map.insert("JAVA_HOME".into(), mapped.to_string_lossy().into_owned());
        }
    }

    // Map GOROOT (absolute) into bundle payload to make trimmed Go toolchains work.
    if let Some(go_root) = env_map.get("GOROOT").cloned() {
        if go_root.starts_with('/') {
            let mut mapped = bundle_root.to_path_buf();
            mapped.push("payload");
            mapped.push(go_root.trim_start_matches('/'));
            env_map.insert("GOROOT".into(), mapped.to_string_lossy().into_owned());
        }
    }

    if run_mode == RunMode::Host {
        remap_host_env_path(bundle_root, &mut env_map, "PYTHONHOME");
        remap_host_env_path_list(bundle_root, &mut env_map, "PYTHONPATH");
    }

    let mut mapped_path_entries: Vec<String> = Vec::new();
    if run_mode == RunMode::Host {
        if let Some(path) = env_map.get("PATH").cloned() {
            mapped_path_entries = remap_path_entries(bundle_root, run_mode, &path);
            if !mapped_path_entries.is_empty() {
                let mut combined = mapped_path_entries.join(":");
                if !path.is_empty() {
                    combined.push(':');
                    combined.push_str(&path);
                }
                env_map.insert("PATH".into(), combined);
            }
        }
    } else {
        // bwrap/chroot：确保 PATH 至少包含常见宿主目录，避免找不到 bwrap。
        let default_path = env_map
            .get("PATH")
            .cloned()
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| {
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into()
            });
        env_map.insert("PATH".into(), default_path.clone());
        mapped_path_entries = default_path
            .split(':')
            .filter(|p| !p.is_empty())
            .map(|s| s.to_string())
            .collect();
    }

    if !library_paths.is_empty() {
        let mut entries: Vec<String> = library_paths
            .iter()
            .map(|path| map_bundle_path(bundle_root, path, run_mode))
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        if let Some(java_home) = env_map.get("JAVA_HOME") {
            let jh = map_env_path(java_home, run_mode);
            entries.push(jh.join("lib").to_string_lossy().into_owned());
            entries.push(jh.join("lib/server").to_string_lossy().into_owned());
        }
        // Heuristic: for each PATH bin entry, also add sibling lib directories to help JVM loads.
        for bin in &mapped_path_entries {
            let bin_path = Path::new(bin);
            if let Some(parent) = bin_path.parent() {
                entries.push(parent.join("lib").to_string_lossy().into_owned());
                entries.push(parent.join("lib/server").to_string_lossy().into_owned());
            }
        }
        dedup_strings(&mut entries);
        let joined_new = entries.join(":");
        if let Some(existing) = env_map.get("LD_LIBRARY_PATH").cloned() {
            let mut combined = String::new();
            combined.push_str(&joined_new);
            if !existing.is_empty() {
                if !combined.is_empty() {
                    combined.push(':');
                }
                combined.push_str(&existing);
            }
            env_map.insert("LD_LIBRARY_PATH".into(), combined);
        } else {
            env_map.insert("LD_LIBRARY_PATH".into(), joined_new);
        }
    }

    let mut block = Vec::new();
    for (key, value) in env_map {
        let mut pair = key;
        pair.push('=');
        pair.push_str(&value);
        block.push(CString::new(pair).map_err(|err| anyhow!("invalid env: {err}"))?);
    }
    Ok(block)
}

fn remap_path_entries(bundle_root: &Path, mode: RunMode, path: &str) -> Vec<String> {
    path.split(':')
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .map(|p| {
            map_bundle_path(bundle_root, &p, mode)
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

fn remap_host_env_path(bundle_root: &Path, env_map: &mut BTreeMap<String, String>, key: &str) {
    let Some(value) = env_map.get(key).cloned() else {
        return;
    };
    if !value.starts_with('/') {
        return;
    }
    let path = PathBuf::from(&value);
    let payload_prefix = bundle_root.join("payload");
    if path.starts_with(&payload_prefix) || path.starts_with(bundle_root) {
        return;
    }
    let mut mapped = payload_prefix;
    mapped.push(value.trim_start_matches('/'));
    env_map.insert(key.to_string(), mapped.to_string_lossy().into_owned());
}

fn remap_host_env_path_list(bundle_root: &Path, env_map: &mut BTreeMap<String, String>, key: &str) {
    let Some(value) = env_map.get(key).cloned() else {
        return;
    };
    if value.is_empty() {
        return;
    }
    let payload_prefix = bundle_root.join("payload");
    let remapped: Vec<String> = value
        .split(':')
        .filter(|p| !p.is_empty())
        .map(|entry| {
            if !entry.starts_with('/') {
                return entry.to_string();
            }
            let p = PathBuf::from(entry);
            if p.starts_with(&payload_prefix) || p.starts_with(bundle_root) {
                return entry.to_string();
            }
            let mut mapped = payload_prefix.clone();
            mapped.push(entry.trim_start_matches('/'));
            mapped.to_string_lossy().into_owned()
        })
        .collect();
    if !remapped.is_empty() {
        env_map.insert(key.to_string(), remapped.join(":"));
    }
}

fn dedup_strings(values: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    values.retain(|v| seen.insert(v.clone()));
}

fn map_bundle_path(bundle_root: &Path, rel: &Path, mode: RunMode) -> PathBuf {
    match mode {
        RunMode::Host => {
            if rel.is_absolute() {
                // For host mode we still want the traced absolute paths to resolve to the
                // bundled payload rather than the host filesystem.
                let stripped = rel.strip_prefix("/").unwrap_or(rel);
                bundle_root.join(stripped)
            } else {
                bundle_root.join(rel)
            }
        }
        RunMode::Bwrap | RunMode::Chroot => {
            if rel.is_absolute() {
                return rel.to_path_buf();
            }
            let mut comps = rel.components();
            if let Some(first) = comps.next() {
                if first.as_os_str() == "payload" {
                    let mut rebuilt = PathBuf::from("/");
                    for c in comps {
                        rebuilt.push(c.as_os_str());
                    }
                    return rebuilt;
                }
            }
            let mut rebuilt = PathBuf::from("/");
            rebuilt.push(rel);
            rebuilt
        }
    }
}

fn ensure_payload_data(bundle_root: &Path, payload_root: &Path) -> Result<()> {
    if let Err(err) = isolate_mount_namespace() {
        eprintln!("sidebundle launcher: warning: failed to isolate mount namespace ({err})");
    }

    let source = bundle_root.join("data");
    if !source.exists() {
        return Ok(());
    }
    let dest = payload_root.join("data");
    if dest.exists() {
        // If it already has contents, assume a previous run populated it.
        if dest
            .read_dir()
            .map(|mut it| it.next().is_some())
            .unwrap_or(false)
        {
            return Ok(());
        }
    }

    fs::create_dir_all(&dest)
        .with_context(|| format!("failed to prepare chroot data dir {}", dest.display()))?;

    // Prefer a bind mount to avoid duplicating data; fall back to hardlink/copy if not permitted.
    let source_c = os_to_cstring(source.as_os_str())?;
    let dest_c = os_to_cstring(dest.as_os_str())?;
    unsafe {
        if libc::mount(
            source_c.as_ptr(),
            dest_c.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND | libc::MS_REC,
            std::ptr::null(),
        ) == 0
        {
            return Ok(());
        }
    }
    let mount_err = std::io::Error::last_os_error();
    mirror_data_tree(&source, &dest).with_context(|| {
        format!("failed to mirror data into chroot (bind mount failed: {mount_err})")
    })
}

fn mirror_data_tree(source: &Path, dest: &Path) -> Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dest.join(entry.file_name());
        let ftype = entry.file_type()?;
        if ftype.is_dir() {
            fs::create_dir_all(&dst_path)?;
            mirror_data_tree(&src_path, &dst_path)?;
        } else if ftype.is_symlink() {
            let target = fs::read_link(&src_path)?;
            if dst_path.exists() {
                let _ = fs::remove_file(&dst_path);
            }
            symlink(target, &dst_path)?;
        } else {
            // Try hardlink first to save space; fall back to copy.
            match fs::hard_link(&src_path, &dst_path) {
                Ok(_) => {}
                Err(_) => {
                    fs::copy(&src_path, &dst_path)?;
                }
            }
        }
    }
    Ok(())
}

/// Best-effort: isolate into a fresh mount namespace and make mounts private so bind mounts
/// (payload/data) don't leak back to the host namespace.
fn isolate_mount_namespace() -> Result<()> {
    unsafe {
        if libc::unshare(libc::CLONE_NEWNS) != 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| "failed to unshare mount namespace");
        }
        let root = CString::new("/")?;
        if libc::mount(
            std::ptr::null(),
            root.as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        ) != 0
        {
            return Err(io::Error::last_os_error())
                .with_context(|| "failed to set mount propagation to private");
        }
    }
    Ok(())
}

fn map_env_path(value: &str, mode: RunMode) -> PathBuf {
    let p = Path::new(value);
    if !p.is_absolute() {
        return p.to_path_buf();
    }
    match mode {
        RunMode::Host => p.to_path_buf(),
        RunMode::Bwrap | RunMode::Chroot => Path::new("/").join(p.strip_prefix("/").unwrap_or(p)),
    }
}

fn find_bwrap() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("SIDEBUNDLE_BWRAP") {
        let candidate = PathBuf::from(path);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':').filter(|p| !p.is_empty()) {
            let candidate = PathBuf::from(dir).join("bwrap");
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    for candidate in ["/usr/bin/bwrap", "/usr/local/bin/bwrap", "/bin/bwrap"] {
        let path = PathBuf::from(candidate);
        if path.exists() {
            return Some(path);
        }
    }
    None
}

fn os_to_cstring(value: &OsStr) -> Result<CString> {
    CString::new(value.as_bytes()).map_err(|err| anyhow!("invalid string: {err}"))
}

fn exec_static(entry: &Path, argv: &[CString], envp: &[CString]) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    use std::ptr;

    let entry_cstr =
        CString::new(entry.as_os_str().as_bytes()).map_err(|err| anyhow!("invalid path: {err}"))?;
    let mut argv_ptrs: Vec<*const libc::c_char> = argv.iter().map(|arg| arg.as_ptr()).collect();
    argv_ptrs.push(ptr::null());
    let mut env_ptrs: Vec<*const libc::c_char> = envp.iter().map(|env| env.as_ptr()).collect();
    env_ptrs.push(ptr::null());

    unsafe {
        libc::execve(entry_cstr.as_ptr(), argv_ptrs.as_ptr(), env_ptrs.as_ptr());
    }
    Err(std::io::Error::last_os_error())
        .with_context(|| format!("execve failed for {}", entry.display()))
}

fn exec_dynamic(linker: &Path, entry: &Path, argv: &[CString], envp: &[CString]) -> Result<()> {
    use std::ptr;

    let linker_cstr = os_to_cstring(linker.as_os_str())?;
    let entry_cstr = os_to_cstring(entry.as_os_str())?;

    let mut argv_ptrs: Vec<*const libc::c_char> = Vec::with_capacity(argv.len() + 2);
    // Keep argv[0] as the entry to satisfy multi-call binaries that validate argv0.
    argv_ptrs.push(entry_cstr.as_ptr());
    argv_ptrs.push(entry_cstr.as_ptr());
    for arg in argv.iter().skip(1) {
        argv_ptrs.push(arg.as_ptr());
    }
    argv_ptrs.push(ptr::null());

    let mut env_ptrs: Vec<*const libc::c_char> = envp.iter().map(|env| env.as_ptr()).collect();
    env_ptrs.push(ptr::null());

    unsafe {
        libc::execve(linker_cstr.as_ptr(), argv_ptrs.as_ptr(), env_ptrs.as_ptr());
    }
    Err(std::io::Error::last_os_error()).with_context(|| {
        format!(
            "execve failed for linker {} (entry {})",
            linker.display(),
            entry.display()
        )
    })
}

fn exec_bwrap(
    bundle_root: &Path,
    payload_root: &Path,
    entry: &Path,
    argv: &[CString],
    envp: &[CString],
) -> Result<()> {
    let bwrap_bin = find_bwrap()
        .context("bubblewrap (bwrap) not found in PATH; required for run_mode=bwrap")?;

    let mut args: Vec<CString> = vec![
        os_to_cstring(bwrap_bin.as_os_str())?,
        CString::new("--bind")?,
        CString::new(payload_root.to_string_lossy().to_string())?,
        CString::new("/")?,
    ];
    let data_root = bundle_root.join("data");
    if data_root.exists() {
        // ensure target mountpoint exists under new root (/data)
        let _ = std::fs::create_dir_all(payload_root.join("data"));
        args.push(CString::new("--bind")?);
        args.push(CString::new(data_root.to_string_lossy().to_string())?);
        args.push(CString::new("/data")?);
    }
    args.push(CString::new("--proc")?);
    args.push(CString::new("/proc")?);
    // minimal device/tempo mounts
    args.push(CString::new("--dev-bind")?);
    args.push(CString::new("/dev/null")?);
    args.push(CString::new("/dev/null")?);
    args.push(CString::new("--dev-bind")?);
    args.push(CString::new("/dev/zero")?);
    args.push(CString::new("/dev/zero")?);
    args.push(CString::new("--dev-bind")?);
    args.push(CString::new("/dev/tty")?);
    args.push(CString::new("/dev/tty")?);
    args.push(CString::new("--dev-bind")?);
    args.push(CString::new("/dev/urandom")?);
    args.push(CString::new("/dev/urandom")?);
    args.push(CString::new("--tmpfs")?);
    args.push(CString::new("/tmp")?);
    args.push(CString::new("--tmpfs")?);
    args.push(CString::new("/run")?);
    args.push(CString::new("--tmpfs")?);
    args.push(CString::new("/dev/shm")?);
    // DNS/hosts
    for file in ["/etc/resolv.conf", "/etc/hosts"] {
        args.push(CString::new("--ro-bind")?);
        args.push(CString::new(file)?);
        args.push(CString::new(file)?);
    }
    args.push(CString::new("--die-with-parent")?);
    args.push(CString::new("--unshare-all")?);
    args.push(CString::new("--")?);

    // For bwrap/chroot we rely on the kernel to use the bundled PT_INTERP inside the sandbox.
    args.push(os_to_cstring(entry.as_os_str())?);
    for arg in argv.iter().skip(1) {
        args.push(arg.clone());
    }

    let mut argv_ptrs: Vec<*const libc::c_char> = args.iter().map(|c| c.as_ptr()).collect();
    argv_ptrs.push(std::ptr::null());
    let mut env_ptrs: Vec<*const libc::c_char> = envp.iter().map(|e| e.as_ptr()).collect();
    env_ptrs.push(std::ptr::null());

    unsafe {
        libc::execve(args[0].as_ptr(), argv_ptrs.as_ptr(), env_ptrs.as_ptr());
    }
    Err(std::io::Error::last_os_error()).with_context(|| "execve failed for bubblewrap launcher")
}

fn exec_chroot(
    bundle_root: &Path,
    payload_root: &Path,
    entry: &Path,
    argv: &[CString],
    envp: &[CString],
) -> Result<()> {
    ensure_payload_data(bundle_root, payload_root)?;
    unsafe {
        let root_c = os_to_cstring(payload_root.as_os_str())?;
        if libc::chroot(root_c.as_ptr()) != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| "chroot failed for launcher");
        }
        if libc::chdir(c"/".as_ptr() as *const libc::c_char) != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| "chdir after chroot failed");
        }
    }
    // Once chrooted, the PT_INTERP inside the payload points to bundled ld-linux; exec the entry
    // directly so /proc/self/exe matches the intended binary (important for multi-call binaries).
    exec_static(entry, argv, envp)
}
