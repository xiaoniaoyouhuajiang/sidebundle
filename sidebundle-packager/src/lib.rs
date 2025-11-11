use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;

use log::info;
use sidebundle_core::{BundleSpec, DependencyClosure, EntryBundlePlan};
use thiserror::Error;

/// 负责把闭包内容写入输出目录并生成 launcher。
#[derive(Debug, Clone)]
pub struct Packager {
    output_root: PathBuf,
}

impl Default for Packager {
    fn default() -> Self {
        Self {
            output_root: PathBuf::from("target/bundles"),
        }
    }
}

impl Packager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_output_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.output_root = root.into();
        self
    }

    pub fn emit(
        &self,
        spec: &BundleSpec,
        closure: &DependencyClosure,
    ) -> Result<PathBuf, PackagerError> {
        if closure.files.is_empty() || closure.entry_plans.is_empty() {
            return Err(PackagerError::EmptyClosure(spec.name.clone()));
        }

        let bundle_root = self.output_root.join(spec.name());
        if bundle_root.exists() {
            fs::remove_dir_all(&bundle_root).map_err(|source| PackagerError::Io {
                path: bundle_root.clone(),
                source,
            })?;
        }
        fs::create_dir_all(bundle_root.join("bin")).map_err(|source| PackagerError::Io {
            path: bundle_root.join("bin"),
            source,
        })?;

        for file in &closure.files {
            let dest_path = bundle_root.join(&file.destination);
            if let Some(parent) = dest_path.parent() {
                fs::create_dir_all(parent).map_err(|source| PackagerError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
            fs::copy(&file.source, &dest_path).map_err(|source| PackagerError::Io {
                path: dest_path.clone(),
                source,
            })?;
            #[cfg(unix)]
            {
                if let Ok(meta) = fs::metadata(&file.source) {
                    let perms = meta.permissions();
                    fs::set_permissions(&dest_path, perms).ok();
                }
            }
        }

        for plan in &closure.entry_plans {
            let launcher_path = bundle_root.join("bin").join(&plan.display_name);
            if let Some(parent) = launcher_path.parent() {
                fs::create_dir_all(parent).map_err(|source| PackagerError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
            let mut file = File::create(&launcher_path).map_err(|source| PackagerError::Io {
                path: launcher_path.clone(),
                source,
            })?;
            file.write_all(render_launcher(plan).as_bytes())
                .map_err(|source| PackagerError::Io {
                    path: launcher_path.clone(),
                    source,
                })?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = fs::metadata(&launcher_path)
                    .map_err(|source| PackagerError::Io {
                        path: launcher_path.clone(),
                        source,
                    })?
                    .permissions();
                perms.set_mode(0o755);
                fs::set_permissions(&launcher_path, perms).map_err(|source| PackagerError::Io {
                    path: launcher_path.clone(),
                    source,
                })?;
            }
        }

        info!(
            "bundle `{}` written to {}",
            spec.name(),
            bundle_root.display()
        );
        Ok(bundle_root)
    }
}

fn render_launcher(plan: &EntryBundlePlan) -> String {
    let binary_ref = format!("${{BUNDLE_ROOT}}/{}", plan.binary_destination.display());
    let linker_ref = format!("${{BUNDLE_ROOT}}/{}", plan.linker_destination.display());
    let mut lib_dirs: Vec<String> = plan
        .library_dirs
        .iter()
        .map(|dir| format!("${{BUNDLE_ROOT}}/{}", dir.display()))
        .collect();
    if lib_dirs.is_empty() {
        if let Some(dir) = plan.binary_destination.parent() {
            lib_dirs.push(format!("${{BUNDLE_ROOT}}/{}", dir.display()));
        }
    }
    let joined = lib_dirs.join(":");

    format!(
        r#"#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BUNDLE_ROOT="$(cd "${{SCRIPT_DIR}}/.." && pwd)"

LINKER="{linker}"
BINARY="{binary}"
LIB_PATH="{lib_path}"

exec "${{LINKER}}" --library-path "${{LIB_PATH}}" "${{BINARY}}" "$@"
"#,
        linker = linker_ref,
        binary = binary_ref,
        lib_path = joined
    )
}

#[derive(Debug, Error)]
pub enum PackagerError {
    #[error("bundle `{0}` has no files to package")]
    EmptyClosure(String),
    #[error("IO error at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use sidebundle_core::{BundleEntry, BundleSpec, DependencyClosure, EntryBundlePlan, TargetTriple};

    #[test]
    fn empty_closure_rejected() {
        let spec = BundleSpec::new("demo", TargetTriple::linux_x86_64())
            .with_entry(BundleEntry::new("/bin/echo", "echo"));
        let closure = DependencyClosure::default();
        let packager = Packager::new();
        assert!(packager.emit(&spec, &closure).is_err());
    }

    #[test]
    fn render_launcher_contains_paths() {
        let plan = EntryBundlePlan {
            display_name: "echo".into(),
            binary_source: PathBuf::from("/bin/echo"),
            binary_destination: PathBuf::from("payload/bin/echo"),
            linker_source: PathBuf::from("/lib64/ld-linux-x86-64.so.2"),
            linker_destination: PathBuf::from("payload/lib64/ld-linux-x86-64.so.2"),
            library_dirs: vec![PathBuf::from("payload/lib64")],
        };
        let content = render_launcher(&plan);
        assert!(content.contains("payload/bin/echo"));
        assert!(content.contains("--library-path"));
    }
}
