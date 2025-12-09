use flate2::write::GzEncoder;
use flate2::Compression;
use sha2::{Digest, Sha256};
use sidebundle_shim::{ShimMetadata, ShimTrailer};
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use tar::Builder;
use walkdir::WalkDir;

use crate::PackagerError;

const SHIM_STUB: &[u8] = include_bytes!(env!("SIDEBUNDLE_SHIM_BIN"));

pub(crate) fn write_shims(
    bundle_root: &Path,
    bundle_name: &str,
    entry_names: &[String],
) -> Result<(), PackagerError> {
    if entry_names.is_empty() {
        return Ok(());
    }

    let (archive, digest) = build_archive(bundle_root)?;
    let shims_dir = bundle_root.join("shims");
    fs::create_dir_all(&shims_dir).map_err(|source| PackagerError::Io {
        path: shims_dir.clone(),
        source,
    })?;
    for entry in entry_names {
        let meta = ShimMetadata {
            bundle_name: bundle_name.to_string(),
            entry_name: entry.clone(),
            default_extract_path: format!("~/.cache/sidebundle/{bundle_name}"),
            archive_sha256: digest.clone(),
        };
        let meta_bytes =
            serde_json::to_vec(&meta).map_err(|err| PackagerError::Shim(err.to_string()))?;
        let trailer = ShimTrailer {
            archive_len: u64::try_from(archive.len())
                .map_err(|err: std::num::TryFromIntError| PackagerError::Shim(err.to_string()))?,
            metadata_len: u64::try_from(meta_bytes.len())
                .map_err(|err: std::num::TryFromIntError| PackagerError::Shim(err.to_string()))?,
        }
        .to_bytes();
        let shim_path = shims_dir.join(entry);
        let mut file = File::create(&shim_path).map_err(|source| PackagerError::Io {
            path: shim_path.clone(),
            source,
        })?;
        file.write_all(SHIM_STUB)
            .and_then(|_| file.write_all(&archive))
            .and_then(|_| file.write_all(&meta_bytes))
            .and_then(|_| file.write_all(&trailer))
            .map_err(|source| PackagerError::Io {
                path: shim_path.clone(),
                source,
            })?;
        set_exec_permissions(&shim_path)?;
    }
    Ok(())
}

fn build_archive(bundle_root: &Path) -> Result<(Vec<u8>, String), PackagerError> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    {
        let mut builder = Builder::new(&mut encoder);
        builder.follow_symlinks(false);
        for entry in WalkDir::new(bundle_root).follow_links(false) {
            let entry = entry.map_err(|err| PackagerError::Shim(err.to_string()))?;
            let path = entry.path();
            let rel = path
                .strip_prefix(bundle_root)
                .map_err(|err| PackagerError::Shim(err.to_string()))?;
            if rel.as_os_str().is_empty() {
                continue;
            }
            if rel.starts_with("shims") {
                continue;
            }
            let meta = entry
                .metadata()
                .map_err(|err| PackagerError::Shim(err.to_string()))?;
            if meta.is_dir() {
                builder
                    .append_dir(rel, path)
                    .map_err(|err| PackagerError::Shim(err.to_string()))?;
            } else {
                builder
                    .append_path_with_name(path, rel)
                    .map_err(|err| PackagerError::Shim(err.to_string()))?;
            }
        }
        builder
            .finish()
            .map_err(|err| PackagerError::Shim(err.to_string()))?;
    }
    let archive = encoder
        .finish()
        .map_err(|err| PackagerError::Shim(err.to_string()))?;
    let digest = sha256_hex(&archive);
    Ok((archive, digest))
}

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

fn set_exec_permissions(path: &Path) -> Result<(), PackagerError> {
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
    Ok(())
}
