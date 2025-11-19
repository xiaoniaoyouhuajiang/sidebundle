use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use sidebundle_core::{EntryBundlePlan, Origin, RuntimeMetadata};

use crate::PackagerError;

const LAUNCHER_BYTES: &[u8] = include_bytes!(env!("SIDEBUNDLE_LAUNCHER_BIN"));
const CONFIG_DIR: &str = "launchers";
const BINARY_NAME: &str = ".sidebundle-launcher";
const CONFIG_EXT: &str = "json";

pub fn write_launchers(
    bundle_root: &Path,
    plans: &[EntryBundlePlan],
    metadata: &HashMap<Origin, RuntimeMetadata>,
) -> Result<(), PackagerError> {
    let bin_dir = bundle_root.join("bin");
    fs::create_dir_all(&bin_dir).map_err(|source| PackagerError::Io {
        path: bin_dir.clone(),
        source,
    })?;
    let launcher_path = bin_dir.join(BINARY_NAME);
    fs::write(&launcher_path, LAUNCHER_BYTES).map_err(|source| PackagerError::Io {
        path: launcher_path.clone(),
        source,
    })?;
    set_exec_permissions(&launcher_path)?;

    let config_dir = bundle_root.join(CONFIG_DIR);
    fs::create_dir_all(&config_dir).map_err(|source| PackagerError::Io {
        path: config_dir.clone(),
        source,
    })?;

    for plan in plans {
        let runtime = metadata.get(&plan.origin).cloned();
        write_config(&config_dir, plan, runtime)?;
        link_entry(&bin_dir, plan)?;
    }
    Ok(())
}

#[derive(Serialize)]
struct LauncherConfig {
    dynamic: bool,
    binary: PathBuf,
    linker: Option<PathBuf>,
    library_paths: Vec<PathBuf>,
    metadata: Option<RuntimeMetadata>,
}

fn write_config(
    dir: &Path,
    plan: &EntryBundlePlan,
    metadata: Option<RuntimeMetadata>,
) -> Result<(), PackagerError> {
    let config_path = dir.join(format!("{}.{}", plan.display_name, CONFIG_EXT));
    let config = LauncherConfig {
        dynamic: plan.requires_linker,
        binary: plan.binary_destination.clone(),
        linker: if plan.requires_linker {
            Some(plan.linker_destination.clone())
        } else {
            None
        },
        library_paths: plan.library_dirs.clone(),
        metadata,
    };
    let data = serde_json::to_vec_pretty(&config).map_err(PackagerError::Manifest)?;
    fs::write(&config_path, data).map_err(|source| PackagerError::Io {
        path: config_path.clone(),
        source,
    })?;
    Ok(())
}

fn link_entry(bin_dir: &Path, plan: &EntryBundlePlan) -> Result<(), PackagerError> {
    let entry_path = bin_dir.join(&plan.display_name);
    if entry_path.exists() {
        fs::remove_file(&entry_path).map_err(|source| PackagerError::Io {
            path: entry_path.clone(),
            source,
        })?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        symlink(Path::new(BINARY_NAME), &entry_path).map_err(|source| PackagerError::Io {
            path: entry_path.clone(),
            source,
        })?;
        return Ok(());
    }
    #[cfg(not(unix))]
    {
        let target = bin_dir.join(BINARY_NAME);
        fs::copy(&target, &entry_path).map_err(|source| PackagerError::Io {
            path: entry_path.clone(),
            source,
        })?;
        set_exec_permissions(&entry_path)?;
        Ok(())
    }
}

fn set_exec_permissions(path: &Path) -> Result<(), PackagerError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)
            .map_err(|source| PackagerError::Io {
                path: path.to_path_buf(),
                source,
            })?
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).map_err(|source| PackagerError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    }
    Ok(())
}
