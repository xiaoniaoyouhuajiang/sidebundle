use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use sidebundle_core::RuntimeMetadata;
use std::collections::BTreeMap;
use std::env;
use std::ffi::{CString, OsStr};
use std::fs;
use std::os::unix::ffi::OsStrExt;
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
    let entry_path = bundle_root.join(&config.binary);
    let argv = build_argv(&entry_path)?;
    let env_block = build_env_block(bundle_root, &config)?;

    if !config.dynamic {
        exec_static(&entry_path, &argv, &env_block)?;
        unreachable!();
    }

    let linker = config
        .linker
        .as_ref()
        .map(|rel| bundle_root.join(rel))
        .ok_or_else(|| anyhow!("dynamic launcher missing linker path"))?;
    exec_dynamic(&linker, &entry_path, &argv, &env_block)?;
    unreachable!();
}

#[derive(Deserialize)]
struct LauncherConfig {
    dynamic: bool,
    binary: PathBuf,
    linker: Option<PathBuf>,
    library_paths: Vec<PathBuf>,
    metadata: Option<RuntimeMetadata>,
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

fn build_argv(entry: &Path) -> Result<Vec<CString>> {
    let mut argv = Vec::new();
    argv.push(os_to_cstring(entry.as_os_str())?);
    for arg in env::args_os().skip(1) {
        argv.push(os_to_cstring(&arg)?);
    }
    Ok(argv)
}

fn build_env_block(bundle_root: &Path, config: &LauncherConfig) -> Result<Vec<CString>> {
    let mut env_map: BTreeMap<String, String> = config
        .metadata
        .as_ref()
        .map(|meta| meta.env.clone())
        .unwrap_or_else(|| env::vars().collect());
    env_map.insert(
        "SIDEBUNDLE_ROOT".into(),
        bundle_root.to_string_lossy().into_owned(),
    );

    if !config.library_paths.is_empty() {
        let joined = config
            .library_paths
            .iter()
            .map(|path| bundle_root.join(path).to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(":");
        env_map.insert("LD_LIBRARY_PATH".into(), joined);
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
    argv_ptrs.push(linker_cstr.as_ptr());
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
