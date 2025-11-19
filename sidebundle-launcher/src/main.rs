use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use sidebundle_core::{AuxvEntry, RuntimeMetadata};
use std::collections::BTreeMap;
use std::env;
use std::ffi::{CString, OsStr};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use userland_execve::{exec_with_options, AuxSnapshot, ExecOptions};

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
    let args = build_argv(&entry_path)?;
    let env_block = build_env_block(bundle_root, &config)?;

    let mut options = ExecOptions::new(&entry_path);
    options.args(args.iter().map(|arg| arg.as_c_str()));
    options.env_pairs(env_block.iter().map(|pair| pair.as_c_str()));

    if let Some(linker) = config.linker.as_ref().map(|rel| bundle_root.join(rel)) {
        options.override_interpreter(Some(linker));
    } else if config.dynamic {
        return Err(anyhow!("dynamic launcher missing linker path"));
    } else {
        options.override_interpreter(None::<&Path>);
    }

    if let Some(metadata) = config.metadata.as_ref() {
        if let Some(snapshot) = build_aux_snapshot(metadata) {
            options.aux_snapshot(snapshot);
        }
    }

    exec_with_options(options);
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

fn build_aux_snapshot(metadata: &RuntimeMetadata) -> Option<AuxSnapshot> {
    if metadata.auxv.is_empty() && metadata.platform.is_none() && metadata.random.is_none() {
        return None;
    }
    let entries = metadata
        .auxv
        .iter()
        .map(|AuxvEntry { key, value }| (*key, *value))
        .collect();
    let snapshot = AuxSnapshot::new(entries)
        .with_platform(metadata.platform.clone())
        .with_random(metadata.random);
    Some(snapshot)
}

fn os_to_cstring(value: &OsStr) -> Result<CString> {
    CString::new(value.as_bytes()).map_err(|err| anyhow!("invalid string: {err}"))
}
