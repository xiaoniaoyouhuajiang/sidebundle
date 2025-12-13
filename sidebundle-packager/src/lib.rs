use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::fmt::Write as FmtWrite;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use log::{debug, info, warn};
#[cfg(target_os = "linux")]
use nix::libc;
use path_clean::PathClean;
use pathdiff::diff_paths;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sidebundle_core::{BundleSpec, DependencyClosure, TracedFile};
use thiserror::Error;

mod launcher;
mod shim;
use launcher::write_launchers;
use shim::write_shims;

/// Writes the dependency closure to disk and generates launchers.
#[derive(Debug, Clone)]
pub struct Packager {
    output_root: PathBuf,
    copy_system_assets: bool,
    emit_shim: bool,
}

impl Default for Packager {
    fn default() -> Self {
        Self {
            output_root: PathBuf::from("target/bundles"),
            copy_system_assets: true,
            emit_shim: false,
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

    /// Emit self-extracting shim executables alongside the bundle.
    pub fn with_shim_output(mut self, enabled: bool) -> Self {
        self.emit_shim = enabled;
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
            #[cfg(target_os = "linux")]
            {
                cleanup_mounts(&bundle_root);
            }
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
        // ensure payload has /data so runtime bind of bundle_root/data -> /data succeeds
        fs::create_dir_all(bundle_root.join("payload/data")).map_err(|source| {
            PackagerError::Io {
                path: bundle_root.join("payload/data"),
                source,
            }
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
        let script_targets: HashSet<PathBuf> = closure
            .entry_plans
            .iter()
            .filter_map(|plan| match plan {
                sidebundle_core::EntryBundlePlan::Script(script) => {
                    Some(script.script_destination.clone())
                }
                _ => None,
            })
            .collect();
        let traced_queue: Vec<TracedFile> = closure.traced_files.clone();
        let mut seen_destinations: HashSet<PathBuf> = HashSet::new();
        let mut alias_file_count: u64 = 0;
        let mut alias_logical_bytes: u64 = 0;
        let mut alias_allocated_bytes: u64 = 0;

        for file in &closure.files {
            let mut source_path = file.source.clone();
            let mut digest = file.digest.clone();

            // If resolv.conf is empty in the image/rootfs, fall back to host copy to avoid empty placeholder.
            if is_empty_resolv_conf(&source_path, &file.destination) {
                if let Some(host_resolv) = fallback_host_resolv_conf() {
                    debug!(
                        "packager: replacing empty resolv.conf {} with host {}",
                        source_path.display(),
                        host_resolv.display()
                    );
                    source_path = host_resolv;
                    digest = compute_digest(&source_path)?;
                } else {
                    warn!(
                        "packager: empty resolv.conf at {}; host fallback missing, keeping empty file",
                        source_path.display()
                    );
                }
            }

            if !source_path.exists() {
                warn!(
                    "packager: source file {} missing; bundle path {}",
                    source_path.display(),
                    file.destination.display()
                );
            } else {
                debug!(
                    "packager: staging {} -> {}",
                    source_path.display(),
                    file.destination.display()
                );
            }
            let stored = store_in_data(&data_dir, &source_path, &digest)?;

            let normalized_destination = normalize_payload_path(&file.destination);
            if !seen_destinations.insert(normalized_destination.clone()) {
                debug!(
                    "packager: skipping duplicate destination {} (normalized from {})",
                    normalized_destination.display(),
                    file.destination.display()
                );
                let _ = alias_map.remove(&file.source);
                continue;
            }

            let dest_path = bundle_root.join(&normalized_destination);
            let mut force_copy = script_targets.contains(&normalized_destination);
            if is_system_resolv_or_hosts(&normalized_destination) {
                force_copy = true;
            }
            match link_or_copy(&stored, &dest_path, !force_copy) {
                Ok(()) => {}
                Err(PackagerError::Io { path, source })
                    if source.kind() == io::ErrorKind::TooManyLinks =>
                {
                    warn!(
                        "packager: TooManyLinks at {}, forcing plain copy",
                        path.display()
                    );
                    if path.exists() {
                        let _ = fs::remove_file(&path);
                    }
                    fs::copy(&stored, &path).map_err(|source| PackagerError::Io {
                        path: path.clone(),
                        source,
                    })?;
                    copy_permissions(&stored, &path).ok();
                }
                Err(e) => return Err(e),
            }
            manifest_files.push(ManifestFile {
                origin: FileOrigin::Dependency,
                source: source_path.display().to_string(),
                destination: normalized_destination.clone(),
                digest: digest.clone(),
            });
            if let Some(runtime_aliases) = alias_map.remove(&file.source) {
                let canonical_abs = bundle_root.join(&normalized_destination);
                for runtime in runtime_aliases {
                    let alias_rel = payload_alias_destination(&runtime);
                    let canonical_rel = clean_relative(&canonical_abs, &bundle_root);
                    let alias_rel = normalize_payload_path(&alias_rel);
                    if alias_rel == canonical_rel {
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
                    fs::copy(&canonical_abs, &alias_abs).map_err(|source| PackagerError::Io {
                        path: alias_abs.clone(),
                        source,
                    })?;
                    copy_permissions(&canonical_abs, &alias_abs).ok();
                    alias_file_count = alias_file_count.saturating_add(1);
                    match fs::metadata(&alias_abs) {
                        Ok(meta) => {
                            alias_logical_bytes = alias_logical_bytes.saturating_add(meta.len());
                            alias_allocated_bytes =
                                alias_allocated_bytes.saturating_add(allocated_bytes(&meta));
                        }
                        Err(_) => {
                            warn!(
                                "packager: failed to stat alias destination {}",
                                alias_abs.display()
                            );
                        }
                    }
                    debug!(
                        "packager: aliasing {} -> {}",
                        canonical_abs.display(),
                        alias_abs.display()
                    );
                }
            }
        }

        info!(
            "packager: alias files: {} (logical={} bytes, allocated={} bytes)",
            alias_file_count, alias_logical_bytes, alias_allocated_bytes
        );

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
            debug!(
                "packager: ignoring symlink {} -> {} (symlink emission disabled)",
                link.destination.display(),
                link.bundle_target.display()
            );
        }

        write_launchers(&bundle_root, &closure.entry_plans, &closure.metadata)?;
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
            link_or_copy(&stored, &dest_path, true)?;
            traced_manifest.push(ManifestFile {
                origin: FileOrigin::Trace,
                source: traced.original.display().to_string(),
                destination,
                digest,
            });
        }

        ensure_runtime_shims(&bundle_root)?;

        write_manifest(
            &bundle_root,
            Manifest {
                name: spec.name().to_string(),
                target: spec.target().as_str().to_string(),
                files: manifest_files,
                traced_files: traced_manifest,
            },
        )?;

        if self.emit_shim {
            let entries: Vec<String> = closure
                .entry_plans
                .iter()
                .map(|plan| plan.display_name().to_string())
                .collect();
            write_shims(&bundle_root, spec.name(), &entries)?;
        }

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
    #[error("shim generation failed: {0}")]
    Shim(String),
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
    let source = resolve_symlink(source);
    if let Some(parent) = stored.parent() {
        fs::create_dir_all(parent).map_err(|source| PackagerError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    fs::copy(&source, &stored).map_err(|source| PackagerError::Io {
        path: stored.clone(),
        source,
    })?;
    copy_permissions(&source, &stored).ok();
    Ok(stored)
}

fn link_or_copy(stored: &Path, dest: &Path, allow_symlink: bool) -> Result<(), PackagerError> {
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
    // Prefer a hardlink so binaries that inspect /proc/self/exe (e.g., busybox) see the expected
    // payload path instead of the hashed data path.
    if fs::hard_link(stored, dest).is_ok() {
        copy_permissions(stored, dest).ok();
        return Ok(());
    }
    if allow_symlink {
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
    }
    match fs::copy(stored, dest) {
        Ok(_) => {}
        Err(ref e) if e.kind() == io::ErrorKind::TooManyLinks => {
            warn!(
                "packager: TooManyLinks copying {} -> {}, falling back to plain copy",
                stored.display(),
                dest.display()
            );
            if dest.exists() {
                let _ = fs::remove_file(dest);
            }
            fs::copy(stored, dest).map_err(|source| PackagerError::Io {
                path: dest.to_path_buf(),
                source,
            })?;
        }
        Err(source) => {
            return Err(PackagerError::Io {
                path: dest.to_path_buf(),
                source,
            })
        }
    }
    copy_permissions(stored, dest).ok();
    Ok(())
}

#[cfg(target_os = "linux")]
fn cleanup_mounts(bundle_root: &Path) {
    for rel in ["payload/data", "payload"] {
        let target = bundle_root.join(rel);
        if !target.exists() {
            continue;
        }
        let c_path = match CString::new(target.as_os_str().as_bytes()) {
            Ok(c) => c,
            Err(_) => continue,
        };
        unsafe {
            // MNT_DETACH so we don't care about active users; ignore errors.
            let _ = libc::umount2(c_path.as_ptr(), libc::MNT_DETACH);
        }
    }
}

/// Resolve a symlink source to its target to avoid embedding host symlink structure
/// (e.g., /etc/resolv.conf -> /run/systemd/resolve/stub-resolv.conf) inside the bundle.
fn resolve_symlink(path: &Path) -> PathBuf {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            if let Ok(target) = fs::read_link(path) {
                if target.is_absolute() {
                    return target;
                } else if let Some(parent) = path.parent() {
                    return parent.join(target);
                }
            }
            path.to_path_buf()
        }
        _ => path.to_path_buf(),
    }
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

fn allocated_bytes(meta: &fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        meta.blocks().saturating_mul(512)
    }
    #[cfg(not(unix))]
    {
        meta.len()
    }
}

fn clean_relative(path: &Path, root: &Path) -> PathBuf {
    path.strip_prefix(root).unwrap_or(path).clean()
}

fn normalize_payload_path(path: &Path) -> PathBuf {
    path.clean()
}

fn is_empty_resolv_conf(source: &Path, destination: &Path) -> bool {
    let dest_is_resolv = is_system_resolv(destination);
    if !dest_is_resolv {
        return false;
    }
    match fs::metadata(source) {
        Ok(meta) => meta.len() == 0,
        Err(_) => false,
    }
}

fn fallback_host_resolv_conf() -> Option<PathBuf> {
    let host = PathBuf::from("/etc/resolv.conf");
    match fs::metadata(&host) {
        Ok(meta) if meta.is_file() && meta.len() > 0 => Some(host),
        _ => None,
    }
}

fn is_system_resolv_or_hosts(path: &Path) -> bool {
    is_system_resolv(path) || is_system_hosts(path)
}

fn is_system_resolv(path: &Path) -> bool {
    tail_matches(path, &["resolv.conf", "etc", "payload"])
}

fn is_system_hosts(path: &Path) -> bool {
    tail_matches(path, &["hosts", "etc", "payload"])
}

fn tail_matches(path: &Path, segments: &[&str]) -> bool {
    let parts: Vec<String> = path
        .components()
        .rev()
        .take(segments.len())
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    if parts.len() != segments.len() {
        return false;
    }
    for (got, expect) in parts.iter().zip(segments.iter()) {
        if got != expect {
            return false;
        }
    }
    true
}

fn compute_digest(path: &Path) -> Result<String, PackagerError> {
    let real = resolve_symlink(path);
    let mut file = File::open(&real).map_err(|source| PackagerError::Io {
        path: real.clone(),
        source,
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = file.read(&mut buffer).map_err(|source| PackagerError::Io {
            path: real.clone(),
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
        FmtWrite::write_fmt(&mut hex, format_args!("{byte:02x}")).expect("write digest");
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
        DeviceNodeSpec {
            rel_path: "payload/dev/zero",
            major: 1,
            minor: 5,
            mode: 0o666,
        },
        DeviceNodeSpec {
            rel_path: "payload/dev/urandom",
            major: 1,
            minor: 9,
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

/// Bundle-time shims for runtime expectations (device nodes, interpreter aliases, etc.).
fn ensure_runtime_shims(bundle_root: &Path) -> Result<(), PackagerError> {
    ensure_device_nodes(bundle_root)?;
    // Data-driven alias list for common interpreter names.
    const ALIASES: &[(&str, &str)] = &[
        // pip shebang commonly points at /usr/bin/python3; ensure it exists if python3.10 is present.
        ("payload/usr/bin/python3", "payload/usr/bin/python3.10"),
    ];
    ensure_aliases(bundle_root, ALIASES)
}

fn ensure_aliases(bundle_root: &Path, aliases: &[(&str, &str)]) -> Result<(), PackagerError> {
    for (dst_rel, target_rel) in aliases {
        let dst = bundle_root.join(dst_rel);
        if dst.exists() {
            continue;
        }
        let target = bundle_root.join(target_rel);
        if !target.exists() {
            continue;
        }
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).map_err(|source| PackagerError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let relative = dst
                .parent()
                .and_then(|p| diff_paths(&target, p))
                .unwrap_or_else(|| target.clone());
            symlink(&relative, &dst).map_err(|source| PackagerError::Io {
                path: dst.clone(),
                source,
            })?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn create_device_node(path: &Path, major: u64, minor: u64, mode: u32) -> io::Result<()> {
    use nix::sys::stat::{makedev, mknod, Mode, SFlag};

    let dev = makedev(major, minor);
    let mode = Mode::from_bits_truncate(mode);
    mknod(path, SFlag::S_IFCHR, mode, dev).map_err(|err| io::Error::other(err.to_string()))
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
    use sidebundle_core::{BundleSpec, DependencyClosure, TargetTriple};
    use sidebundle_shim::{ShimTrailer, TRAILER_SIZE};
    use std::io::{Read, Seek, SeekFrom};
    use tempfile::tempdir;

    #[test]
    fn empty_closure_rejected() {
        let spec = BundleSpec::new("demo", TargetTriple::linux_x86_64())
            .with_entry(BundleSpec::host_entry("/bin/echo", "echo"));
        let closure = DependencyClosure::default();
        let packager = Packager::new();
        assert!(packager.emit(&spec, &closure).is_err());
    }

    #[test]
    fn write_shim_outputs_trailer() {
        let temp = tempdir().unwrap();
        let bundle_root = temp.path().join("bundle");
        fs::create_dir_all(bundle_root.join("bin")).unwrap();
        fs::create_dir_all(bundle_root.join("payload")).unwrap();
        fs::write(bundle_root.join("payload").join("file.txt"), b"hello").unwrap();
        let shim_path = {
            let entries = vec!["hello".to_string()];
            shim::write_shims(&bundle_root, "demo", &entries).unwrap();
            bundle_root.join("shims/hello")
        };
        assert!(shim_path.exists());
        let mut file = File::open(&shim_path).unwrap();
        let size = file.metadata().unwrap().len();
        assert!(size > TRAILER_SIZE as u64);
        file.seek(SeekFrom::End(-(TRAILER_SIZE as i64))).unwrap();
        let mut buf = [0u8; TRAILER_SIZE];
        file.read_exact(&mut buf).unwrap();
        let trailer = ShimTrailer::from_bytes(&buf).expect("valid trailer");
        assert!(trailer.archive_len > 0);
        assert!(trailer.metadata_len > 0);
    }
}
