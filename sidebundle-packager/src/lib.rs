use std::fmt::Write as FmtWrite;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use log::{info, warn};
use pathdiff::diff_paths;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sidebundle_core::{BundleSpec, DependencyClosure, EntryBundlePlan};
use thiserror::Error;

/// Writes the dependency closure to disk and generates launchers.
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

        let data_dir = bundle_root.join("data");
        fs::create_dir_all(&data_dir).map_err(|source| PackagerError::Io {
            path: data_dir.clone(),
            source,
        })?;

        let mut manifest_files = Vec::new();

        for file in &closure.files {
            let stored = store_in_data(&data_dir, &file.source, &file.digest)?;

            let dest_path = bundle_root.join(&file.destination);
            link_or_copy(&stored, &dest_path)?;
            manifest_files.push(ManifestFile {
                origin: FileOrigin::Dependency,
                source: file.source.display().to_string(),
                destination: file.destination.clone(),
                digest: file.digest.clone(),
            });
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

        let mut traced_manifest = Vec::new();
        for traced in &closure.traced_files {
            match fs::metadata(&traced.resolved) {
                Ok(meta) if meta.is_file() => {}
                Ok(_) => {
                    warn!(
                        "traced path {} is not a regular file, skipping",
                        traced.original.display()
                    );
                    continue;
                }
                Err(err) => {
                    warn!(
                        "failed to read traced path {}: {err}",
                        traced.resolved.display()
                    );
                    continue;
                }
            }
            let digest = compute_digest(&traced.resolved)?;
            let stored = store_in_data(&data_dir, &traced.resolved, &digest)?;
            let destination = traced_destination(&traced.original);
            let dest_path = bundle_root.join(&destination);
            link_or_copy(&stored, &dest_path)?;
            traced_manifest.push(ManifestFile {
                origin: FileOrigin::Trace,
                source: traced.original.display().to_string(),
                destination,
                digest,
            });
        }

        write_manifest(
            &bundle_root,
            Manifest {
                name: spec.name().to_string(),
                target: spec.target().as_str().to_string(),
                files: manifest_files,
                traced_files: traced_manifest,
            },
        )?;

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
    if !plan.requires_linker {
        return format!(
            r#"#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BUNDLE_ROOT="$(cd "${{SCRIPT_DIR}}/.." && pwd)"
export SIDEBUNDLE_ROOT="${{BUNDLE_ROOT}}"

fail() {{
    echo "sidebundle launcher: $1" >&2
    exit 1
}}

BINARY="{binary}"
if [[ ! -x "${{BINARY}}" ]]; then
    fail "entry binary missing or not executable: ${{BINARY}}"
fi
exec "${{BINARY}}" "$@"
"#,
            binary = binary_ref
        );
    }

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
export SIDEBUNDLE_ROOT="${{BUNDLE_ROOT}}"

fail() {{
    echo "sidebundle launcher: $1" >&2
    exit 1
}}

LINKER="{linker}"
BINARY="{binary}"
LIB_PATH="{lib_path}"

if [[ ! -x "${{LINKER}}" ]]; then
    fail "linker missing or not executable: ${{LINKER}}"
fi
if [[ ! -x "${{BINARY}}" ]]; then
    fail "entry binary missing or not executable: ${{BINARY}}"
fi

IFS=':' read -r -a __libdirs <<< "${{LIB_PATH}}"
for dir in "${{__libdirs[@]}}"; do
    if [[ -z "${{dir}}" ]]; then
        continue
    fi
    if [[ ! -d "${{dir}}" ]]; then
        echo "sidebundle launcher: warning: library directory missing: ${{dir}}" >&2
    fi
done

ORIGINAL_LD_LIBRARY_PATH="${{LD_LIBRARY_PATH:-}}"
if [[ -n "${{ORIGINAL_LD_LIBRARY_PATH}}" ]]; then
    export LD_LIBRARY_PATH="${{LIB_PATH}}:${{ORIGINAL_LD_LIBRARY_PATH}}"
else
    export LD_LIBRARY_PATH="${{LIB_PATH}}"
fi
unset LD_PRELOAD

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
    #[error("failed to serialize manifest: {0}")]
    Manifest(serde_json::Error),
}

#[derive(Serialize)]
struct Manifest {
    name: String,
    target: String,
    files: Vec<ManifestFile>,
    traced_files: Vec<ManifestFile>,
}

#[derive(Serialize)]
struct ManifestFile {
    origin: FileOrigin,
    source: String,
    destination: PathBuf,
    digest: String,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum FileOrigin {
    Dependency,
    Trace,
}

fn stored_data_path(data_dir: &Path, digest: &str) -> PathBuf {
    data_dir.join(digest)
}

fn store_in_data(data_dir: &Path, source: &Path, digest: &str) -> Result<PathBuf, PackagerError> {
    let stored = stored_data_path(data_dir, digest);
    if stored.exists() {
        return Ok(stored);
    }
    if let Some(parent) = stored.parent() {
        fs::create_dir_all(parent).map_err(|source| PackagerError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    fs::copy(source, &stored).map_err(|source| PackagerError::Io {
        path: stored.clone(),
        source,
    })?;
    copy_permissions(source, &stored).ok();
    Ok(stored)
}

fn link_or_copy(stored: &Path, dest: &Path) -> Result<(), PackagerError> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|source| PackagerError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    if dest.exists() {
        fs::remove_file(dest).map_err(|source| PackagerError::Io {
            path: dest.to_path_buf(),
            source,
        })?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        if let Some(parent) = dest.parent() {
            if let Some(relative) = diff_paths(stored, parent) {
                if symlink(&relative, dest).is_ok() {
                    return Ok(());
                }
            }
        }
    }
    fs::copy(stored, dest).map_err(|source| PackagerError::Io {
        path: dest.to_path_buf(),
        source,
    })?;
    copy_permissions(stored, dest).ok();
    Ok(())
}

fn copy_permissions(src: &Path, dest: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        if let Ok(meta) = fs::metadata(src) {
            let perms = meta.permissions();
            fs::set_permissions(dest, perms)?;
        }
    }
    Ok(())
}

fn compute_digest(path: &Path) -> Result<String, PackagerError> {
    let mut file = File::open(path).map_err(|source| PackagerError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = file.read(&mut buffer).map_err(|source| PackagerError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        FmtWrite::write_fmt(&mut hex, format_args!("{:02x}", byte)).expect("write digest");
    }
    Ok(hex)
}

fn traced_destination(path: &Path) -> PathBuf {
    let mut dest = PathBuf::from("resources/traced");
    let relative = if path.is_absolute() {
        path.strip_prefix("/").unwrap_or(path)
    } else {
        path
    };
    dest.push(relative);
    dest
}

fn write_manifest(bundle_root: &Path, manifest: Manifest) -> Result<(), PackagerError> {
    let manifest_path = bundle_root.join("manifest.lock");
    let mut file = File::create(&manifest_path).map_err(|source| PackagerError::Io {
        path: manifest_path.clone(),
        source,
    })?;
    serde_json::to_writer_pretty(&mut file, &manifest).map_err(PackagerError::Manifest)?;
    file.write_all(b"\n").map_err(|source| PackagerError::Io {
        path: manifest_path.clone(),
        source,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sidebundle_core::{BundleSpec, DependencyClosure, EntryBundlePlan, TargetTriple};

    #[test]
    fn empty_closure_rejected() {
        let spec = BundleSpec::new("demo", TargetTriple::linux_x86_64())
            .with_entry(BundleSpec::host_entry("/bin/echo", "echo"));
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
            requires_linker: true,
        };
        let content = render_launcher(&plan);
        assert!(content.contains("payload/bin/echo"));
        assert!(content.contains("--library-path"));
        assert!(content.contains("LD_LIBRARY_PATH"));
        assert!(content.contains("unset LD_PRELOAD"));
        assert!(content.contains("SIDEBUNDLE_ROOT"));
        assert!(content.contains("sidebundle launcher"));
        assert!(content.contains("linker missing or not executable"));
    }
}
