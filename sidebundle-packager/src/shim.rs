use flate2::write::GzEncoder;
use flate2::Compression;
use sha2::{Digest, Sha256};
use sidebundle_shim::{ShimMetadata, ShimTrailer};
#[cfg(unix)]
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use tar::{Builder, EntryType, Header};
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
        #[cfg(unix)]
        let mut hardlinks: HashMap<(u64, u64), std::path::PathBuf> = HashMap::new();
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
            let ftype = entry.file_type();
            let meta = entry
                .metadata()
                .map_err(|err| PackagerError::Shim(err.to_string()))?;
            if ftype.is_dir() {
                builder
                    .append_dir(rel, path)
                    .map_err(|err| PackagerError::Shim(err.to_string()))?;
            } else if ftype.is_file() {
                #[cfg(unix)]
                {
                    let key = (meta.dev(), meta.ino());
                    if let Some(first) = hardlinks.get(&key) {
                        let mut header = Header::new_gnu();
                        header.set_entry_type(EntryType::Link);
                        header.set_size(0);
                        header.set_mode(meta.mode());
                        header.set_uid(meta.uid().into());
                        header.set_gid(meta.gid().into());
                        header.set_mtime(u64::try_from(meta.mtime()).unwrap_or(0));
                        builder
                            .append_link(&mut header, rel, first)
                            .map_err(|err| PackagerError::Shim(err.to_string()))?;
                        continue;
                    }
                    hardlinks.insert(key, rel.to_path_buf());
                }
                builder
                    .append_path_with_name(path, rel)
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

#[cfg(test)]
mod tests {
    use super::build_archive;
    use flate2::read::GzDecoder;
    use std::fs;
    use std::io::Read;
    use tar::{Archive, EntryType};
    use tempfile::tempdir;

    #[test]
    fn archive_preserves_hardlinks_as_tar_links() {
        let dir = tempdir().unwrap();
        let bundle_root = dir.path().join("bundle");
        fs::create_dir_all(bundle_root.join("payload")).unwrap();

        let a = bundle_root.join("payload/a.txt");
        let b = bundle_root.join("payload/b.txt");
        fs::write(&a, b"hello").unwrap();
        fs::hard_link(&a, &b).unwrap();

        let (archive_bytes, _digest) = build_archive(&bundle_root).unwrap();
        let mut decoder = GzDecoder::new(&archive_bytes[..]);
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed).unwrap();

        let mut ar = Archive::new(&decompressed[..]);
        let mut file_entries = 0;
        let mut link_entries = 0;
        for entry in ar.entries().unwrap() {
            let entry = entry.unwrap();
            let path = entry.path().unwrap().to_path_buf();
            if path == std::path::Path::new("payload/a.txt")
                || path == std::path::Path::new("payload/b.txt")
            {
                match entry.header().entry_type() {
                    EntryType::Regular => file_entries += 1,
                    EntryType::Link => link_entries += 1,
                    _ => {}
                }
            }
        }
        assert_eq!(file_entries, 1);
        assert_eq!(link_entries, 1);
    }
}
