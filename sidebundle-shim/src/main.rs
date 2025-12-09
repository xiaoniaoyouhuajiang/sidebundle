use anyhow::{anyhow, Context, Result};
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use sidebundle_shim::{ShimMetadata, ShimTrailer, MARKER_FILE, SHIM_MAGIC, TRAILER_SIZE};
use std::env;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use tar::Archive;

fn main() {
    if let Err(err) = run() {
        eprintln!("sidebundle shim: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let exe = env::current_exe().context("failed to resolve shim path")?;
    let mut file = File::open(&exe).context("failed to open shim executable")?;
    let file_size = file.metadata().context("failed to stat shim")?.len();
    if file_size < TRAILER_SIZE as u64 {
        return Err(anyhow!("shim payload missing trailer"));
    }
    file.seek(SeekFrom::End(-(TRAILER_SIZE as i64)))
        .context("failed to seek to trailer")?;
    let mut trailer_buf = [0u8; TRAILER_SIZE];
    file.read_exact(&mut trailer_buf)
        .context("failed to read trailer")?;
    if &trailer_buf[..8] != SHIM_MAGIC {
        return Err(anyhow!("shim payload missing magic header"));
    }
    let trailer = ShimTrailer::from_bytes(&trailer_buf).ok_or_else(|| anyhow!("bad trailer"))?;
    let meta_offset = file_size
        .checked_sub(TRAILER_SIZE as u64 + trailer.metadata_len)
        .ok_or_else(|| anyhow!("shim payload metadata bounds"))?;
    let archive_offset = meta_offset
        .checked_sub(trailer.archive_len)
        .ok_or_else(|| anyhow!("shim payload archive bounds"))?;

    file.seek(SeekFrom::Start(meta_offset))
        .context("seek metadata")?;
    let mut metadata_buf = vec![0u8; trailer.metadata_len as usize];
    file.read_exact(&mut metadata_buf)
        .context("read metadata")?;
    let metadata: ShimMetadata = serde_json::from_slice(&metadata_buf).context("parse metadata")?;

    file.seek(SeekFrom::Start(archive_offset))
        .context("seek archive")?;
    let mut archive_buf = vec![0u8; trailer.archive_len as usize];
    file.read_exact(&mut archive_buf).context("read archive")?;

    let digest = sha256_hex(&archive_buf);
    if digest != metadata.archive_sha256 {
        return Err(anyhow!("archive digest mismatch"));
    }

    let extract_dir = resolve_extract_dir(&metadata)?;
    ensure_dir(&extract_dir)?;
    let marker = extract_dir.join(MARKER_FILE);
    let mut needs_extract = true;
    if env::var("SIDEBUNDLE_FORCE_EXTRACT").ok().as_deref() == Some("1") {
        needs_extract = true;
    } else if let Ok(existing) = fs::read_to_string(&marker) {
        if existing.trim() == digest {
            needs_extract = false;
        }
    }

    if needs_extract {
        let decoder = GzDecoder::new(&archive_buf[..]);
        let mut archive = Archive::new(decoder);
        archive
            .unpack(&extract_dir)
            .with_context(|| format!("failed to unpack bundle into {}", extract_dir.display()))?;
        fs::write(&marker, &digest).context("write marker")?;
    }

    let entry = extract_dir.join("bin").join(&metadata.entry_name);
    exec_entry(&entry)
}

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

fn resolve_extract_dir(meta: &ShimMetadata) -> Result<PathBuf> {
    if let Ok(dir) = env::var("SIDEBUNDLE_EXTRACT_DIR") {
        if !dir.is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }
    Ok(expand_tilde(&meta.default_extract_path))
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(stripped) = path.strip_prefix("~/") {
        if let Ok(home) = env::var("HOME") {
            return PathBuf::from(home).join(stripped);
        }
    }
    PathBuf::from(path)
}

fn ensure_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)
        .with_context(|| format!("failed to create extract dir {}", path.display()))
}

fn exec_entry(path: &Path) -> Result<()> {
    if !path.exists() {
        return Err(anyhow!("entry {} not found", path.display()));
    }
    let mut cmd = Command::new(path);
    cmd.args(env::args_os().skip(1));
    let error = cmd.exec();
    Err(anyhow!("failed to exec {}: {}", path.display(), error))
}
