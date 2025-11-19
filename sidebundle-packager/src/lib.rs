use std::collections::HashMap;
use std::fmt::Write as FmtWrite;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use log::{debug, info, warn};
use pathdiff::diff_paths;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sidebundle_core::{BundleSpec, DependencyClosure, ResolvedSymlink, TracedFile};
use thiserror::Error;

mod launcher;
use launcher::write_launchers;

/// Writes the dependency closure to disk and generates launchers.
#[derive(Debug, Clone)]
pub struct Packager {
    output_root: PathBuf,
    copy_system_assets: bool,
}

impl Default for Packager {
    fn default() -> Self {
        Self {
            output_root: PathBuf::from("target/bundles"),
            copy_system_assets: true,
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

    /// Enable or disable copying of system config assets (/etc/passwd, etc.).
    pub fn with_system_assets(mut self, enabled: bool) -> Self {
        self.copy_system_assets = enabled;
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
        let mut alias_map: HashMap<PathBuf, Vec<PathBuf>> = closure.runtime_aliases.clone();
        for traced in &closure.traced_files {
            alias_map
                .entry(traced.resolved.clone())
                .or_default()
                .push(traced.original.clone());
        }
        let host_assets = if self.copy_system_assets {
            collect_host_system_assets()
        } else {
            HashMap::new()
        };
        let traced_queue: Vec<TracedFile> = closure.traced_files.clone();

        for file in &closure.files {
            if !file.source.exists() {
                warn!(
                    "packager: source file {} missing; bundle path {}",
                    file.source.display(),
                    file.destination.display()
                );
            } else {
                debug!(
                    "packager: staging {} -> {}",
                    file.source.display(),
                    file.destination.display()
                );
            }
            let stored = store_in_data(&data_dir, &file.source, &file.digest)?;

            let dest_path = bundle_root.join(&file.destination);
            link_or_copy(&stored, &dest_path)?;
            manifest_files.push(ManifestFile {
                origin: FileOrigin::Dependency,
                source: file.source.display().to_string(),
                destination: file.destination.clone(),
                digest: file.digest.clone(),
            });
            if let Some(runtime_aliases) = alias_map.remove(&file.source) {
                let canonical_abs = bundle_root.join(&file.destination);
                for runtime in runtime_aliases {
                    let alias_rel = payload_alias_destination(&runtime);
                    if alias_rel == file.destination {
                        continue;
                    }
                    let alias_abs = bundle_root.join(&alias_rel);
                    if alias_abs.exists() {
                        fs::remove_file(&alias_abs).map_err(|source| PackagerError::Io {
                            path: alias_abs.clone(),
                            source,
                        })?;
                    }
                    if let Some(parent) = alias_abs.parent() {
                        fs::create_dir_all(parent).map_err(|source| PackagerError::Io {
                            path: parent.to_path_buf(),
                            source,
                        })?;
                    }
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::symlink;
                        if let Some(parent) = alias_abs.parent() {
                            if let Some(relative) = diff_paths(&canonical_abs, parent) {
                                if symlink(&relative, &alias_abs).is_ok() {
                                    continue;
                                }
                            }
                        }
                    }
                    fs::copy(&canonical_abs, &alias_abs).map_err(|source| PackagerError::Io {
                        path: alias_abs.clone(),
                        source,
                    })?;
                    copy_permissions(&canonical_abs, &alias_abs).ok();
                    debug!(
                        "packager: aliasing {} -> {}",
                        canonical_abs.display(),
                        alias_abs.display()
                    );
                }
            }
        }

        for (missing_source, aliases) in alias_map {
            warn!(
                "packager: traced source {} missing for runtime paths {:?}",
                missing_source.display(),
                aliases
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
            );
        }

        for link in &closure.symlinks {
            write_bundle_symlink(&bundle_root, link)?;
        }

        write_launchers(&bundle_root, &closure.entry_plans)?;
        let mut traced_manifest = Vec::new();
        for traced in &traced_queue {
            let mut source_path = traced.resolved.clone();
            if !source_path.exists() {
                if let Some(host_path) = host_assets.get(&traced.original) {
                    debug!(
                        "packager: using host asset {} for runtime {}",
                        host_path.display(),
                        traced.original.display()
                    );
                    source_path = host_path.clone();
                }
            } else {
                debug!(
                    "packager: staging traced {} (runtime {})",
                    source_path.display(),
                    traced.original.display()
                );
            }
            match fs::metadata(&source_path) {
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
            let digest = compute_digest(&source_path)?;
            let stored = store_in_data(&data_dir, &source_path, &digest)?;
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

        ensure_device_nodes(&bundle_root)?;

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

fn write_bundle_symlink(bundle_root: &Path, link: &ResolvedSymlink) -> Result<(), PackagerError> {
    let dest_path = bundle_root.join(&link.destination);
    if let Some(parent) = dest_path.parent() {
        fs::create_dir_all(parent).map_err(|source| PackagerError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    if dest_path.exists() {
        let meta = fs::symlink_metadata(&dest_path).map_err(|source| PackagerError::Io {
            path: dest_path.clone(),
            source,
        })?;
        if meta.is_dir() {
            debug!(
                "packager: skipping symlink {} because directory already exists",
                dest_path.display()
            );
            return Ok(());
        }
        fs::remove_file(&dest_path).map_err(|source| PackagerError::Io {
            path: dest_path.clone(),
            source,
        })?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let relative_target = relative_symlink_target(bundle_root, &dest_path, &link.bundle_target);
        symlink(&relative_target, &dest_path).map_err(|source| PackagerError::Io {
            path: dest_path.clone(),
            source,
        })?;
        debug!(
            "packager: symlink {} -> {}",
            dest_path.display(),
            relative_target.display()
        );
    }
    #[cfg(not(unix))]
    {
        let source_abs = bundle_root.join(&link.bundle_target);
        fs::copy(&source_abs, &dest_path).map_err(|source| PackagerError::Io {
            path: dest_path.clone(),
            source,
        })?;
        copy_permissions(&source_abs, &dest_path).ok();
    }
    Ok(())
}

fn relative_symlink_target(bundle_root: &Path, dest: &Path, bundle_target: &Path) -> PathBuf {
    let target_abs = bundle_root.join(bundle_target);
    if let Some(parent) = dest.parent() {
        if let Some(relative) = diff_paths(&target_abs, parent) {
            if relative.as_os_str().is_empty() {
                return PathBuf::from(".");
            }
            return relative;
        }
    }
    bundle_target.to_path_buf()
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

fn payload_alias_destination(path: &Path) -> PathBuf {
    let mut dest = PathBuf::from("payload");
    for component in path.components() {
        match component {
            Component::RootDir => {
                dest = PathBuf::from("payload");
            }
            Component::CurDir => continue,
            Component::ParentDir => {
                if dest != Path::new("payload") {
                    dest.pop();
                }
            }
            Component::Normal(part) => dest.push(part),
            Component::Prefix(_) => dest.push(component.as_os_str()),
        }
    }
    dest
}

fn collect_host_system_assets() -> HashMap<PathBuf, PathBuf> {
    let mut map = HashMap::new();
    let candidates = [
        "/etc/passwd",
        "/etc/group",
        "/etc/nsswitch.conf",
        "/etc/resolv.conf",
        "/etc/hosts",
        "/etc/ld.so.cache",
    ];
    for path in candidates {
        let host_path = PathBuf::from(path);
        if host_path.exists() {
            map.insert(PathBuf::from(path), host_path);
        }
    }
    map
}

struct DeviceNodeSpec {
    rel_path: &'static str,
    major: u64,
    minor: u64,
    mode: u32,
}

fn ensure_device_nodes(bundle_root: &Path) -> Result<(), PackagerError> {
    const DEVICES: &[DeviceNodeSpec] = &[
        DeviceNodeSpec {
            rel_path: "payload/dev/null",
            major: 1,
            minor: 3,
            mode: 0o666,
        },
        DeviceNodeSpec {
            rel_path: "payload/dev/tty",
            major: 5,
            minor: 0,
            mode: 0o666,
        },
    ];

    for spec in DEVICES {
        let dest = bundle_root.join(spec.rel_path);
        if dest.exists() {
            continue;
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|source| PackagerError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        match create_device_node(&dest, spec.major, spec.minor, spec.mode) {
            Ok(()) => {
                debug!("created device node {}", dest.display());
            }
            Err(err) => {
                warn!(
                    "failed to create device {} via mknod ({}); writing stub file",
                    dest.display(),
                    err
                );
                File::create(&dest).map_err(|source| PackagerError::Io {
                    path: dest.clone(),
                    source,
                })?;
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn create_device_node(path: &Path, major: u64, minor: u64, mode: u32) -> io::Result<()> {
    use nix::sys::stat::{makedev, mknod, Mode, SFlag};

    let dev = makedev(major, minor);
    let mode = Mode::from_bits_truncate(mode);
    mknod(path, SFlag::S_IFCHR, mode, dev)
        .map_err(|err| io::Error::new(io::ErrorKind::Other, err.to_string()))
}

#[cfg(not(unix))]
fn create_device_node(_path: &Path, _major: u64, _minor: u64, _mode: u32) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Other,
        "device nodes are unsupported on this platform",
    ))
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
}
