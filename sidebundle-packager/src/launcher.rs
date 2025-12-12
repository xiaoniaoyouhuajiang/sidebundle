use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use sidebundle_core::{EntryBundlePlan, Origin, RunMode, RuntimeMetadata};

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
        let runtime = metadata.get(plan.origin()).cloned();
        write_config(&config_dir, plan, runtime)?;
        link_entry(&bin_dir, plan.display_name())?;
    }
    Ok(())
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LauncherConfig {
    Binary {
        dynamic: bool,
        binary: PathBuf,
        linker: Option<PathBuf>,
        library_paths: Vec<PathBuf>,
        metadata: Option<RuntimeMetadata>,
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
        run_mode: RunMode,
    },
}

fn write_config(
    dir: &Path,
    plan: &EntryBundlePlan,
    metadata: Option<RuntimeMetadata>,
) -> Result<(), PackagerError> {
    let config_path = dir.join(format!("{}.{}", plan.display_name(), CONFIG_EXT));
    let config = match plan {
        EntryBundlePlan::Binary(plan) => LauncherConfig::Binary {
            dynamic: plan.requires_linker,
            binary: plan.binary_destination.clone(),
            linker: if plan.requires_linker {
                Some(plan.linker_destination.clone())
            } else {
                None
            },
            library_paths: plan.library_dirs.clone(),
            metadata,
            run_mode: plan.run_mode.unwrap_or(RunMode::Host),
        },
        EntryBundlePlan::Script(plan) => LauncherConfig::Script {
            dynamic: plan.requires_linker,
            interpreter: plan.interpreter_destination.clone(),
            script: plan.script_destination.clone(),
            args: plan.interpreter_args.clone(),
            linker: if plan.requires_linker {
                Some(plan.linker_destination.clone())
            } else {
                None
            },
            library_paths: plan.library_dirs.clone(),
            metadata: inject_script_metadata(plan, metadata),
            run_mode: plan.run_mode.unwrap_or(RunMode::Host),
        },
    };
    let data = serde_json::to_vec_pretty(&config).map_err(PackagerError::Manifest)?;
    fs::write(&config_path, data).map_err(|source| PackagerError::Io {
        path: config_path.clone(),
        source,
    })?;
    Ok(())
}

fn link_entry(bin_dir: &Path, name: &str) -> Result<(), PackagerError> {
    let entry_path = bin_dir.join(name);
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
        Ok(())
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

fn inject_script_metadata(
    plan: &sidebundle_core::ScriptEntryPlan,
    metadata: Option<RuntimeMetadata>,
) -> Option<RuntimeMetadata> {
    if !is_node_interpreter(&plan.interpreter_destination) {
        return metadata;
    }
    let mut metadata = metadata.unwrap_or_default();
    // Debian/Ubuntu package many global JS deps under /usr/share/nodejs. When bundling shebang
    // scripts (e.g. npm), ensure Node can resolve those modules inside the bundle.
    metadata
        .env
        .entry("NODE_PATH".into())
        .or_insert_with(|| "/usr/share/nodejs".to_string());
    let opts = metadata.env.entry("NODE_OPTIONS".into()).or_default();
    const FLAGS: &[&str] = &["--preserve-symlinks-main", "--preserve-symlinks"];
    for flag in FLAGS {
        if !opts.split_whitespace().any(|existing| existing == *flag) {
            if !opts.is_empty() {
                opts.push(' ');
            }
            opts.push_str(flag);
        }
    }
    Some(metadata)
}

fn is_node_interpreter(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| matches!(name, "node" | "nodejs"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sidebundle_core::ScriptEntryPlan;
    use std::path::PathBuf;

    fn script_plan_with_interpreter(interpreter: &str) -> ScriptEntryPlan {
        ScriptEntryPlan {
            display_name: "demo".into(),
            script_source: PathBuf::from("/bin/demo"),
            script_destination: PathBuf::from("payload/bin/demo"),
            interpreter_source: PathBuf::from(interpreter),
            interpreter_destination: PathBuf::from(interpreter),
            linker_source: PathBuf::new(),
            linker_destination: PathBuf::new(),
            interpreter_args: Vec::new(),
            library_dirs: Vec::new(),
            requires_linker: false,
            origin: sidebundle_core::Origin::Host,
            run_mode: None,
        }
    }

    #[test]
    fn node_scripts_get_node_path_default() {
        let plan = script_plan_with_interpreter("/usr/bin/node");
        let meta = inject_script_metadata(&plan, None).unwrap();
        assert_eq!(
            meta.env.get("NODE_PATH").map(String::as_str),
            Some("/usr/share/nodejs")
        );
    }

    #[test]
    fn existing_node_path_is_preserved() {
        let plan = script_plan_with_interpreter("/usr/bin/node");
        let mut meta = RuntimeMetadata::default();
        meta.env.insert("NODE_PATH".into(), "/custom/nodejs".into());
        let meta = inject_script_metadata(&plan, Some(meta)).unwrap();
        assert_eq!(
            meta.env.get("NODE_PATH").map(String::as_str),
            Some("/custom/nodejs")
        );
    }

    #[test]
    fn non_node_scripts_untouched() {
        let plan = script_plan_with_interpreter("/usr/bin/python3");
        assert!(inject_script_metadata(&plan, None).is_none());
    }
}
