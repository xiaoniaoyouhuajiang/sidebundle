use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct LibraryResolution {
    pub name: String,
    pub target: PathBuf,
}

/// `ld.so --list` output, including libraries the linker could not resolve.
/// A missing soname is not fatal by itself: callers may search private directories
/// (`pulseaudio/`, `$ORIGIN`) before failing the pack.
#[derive(Debug, Clone)]
pub struct TraceListing {
    pub resolved: Vec<LibraryResolution>,
    pub missing: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct LinkerRunner {
    cache: Arc<Mutex<HashMap<CacheKey, TraceListing>>>,
}

impl Default for LinkerRunner {
    fn default() -> Self {
        Self {
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl LinkerRunner {
    pub fn new() -> Self {
        Self::default()
    }

    fn resolve_subject(subject: &Path) -> PathBuf {
        // Some ELF behaviors (notably $ORIGIN expansion) depend on the path used to invoke the
        // binary. If the subject is a symlink (e.g. /usr/bin/java -> .../jvm/.../bin/java),
        // invoking the linker with the symlink path can change $ORIGIN and break dependency
        // tracing. Prefer the canonical target when available.
        std::fs::canonicalize(subject).unwrap_or_else(|_| subject.to_path_buf())
    }

    pub fn trace_dependencies(
        &self,
        linker: &Path,
        subject: &Path,
        search_paths: &[PathBuf],
    ) -> Result<Vec<LibraryResolution>, LinkerError> {
        let listing = self.trace_dependencies_partial(linker, subject, search_paths)?;
        if let Some(name) = listing.missing.first() {
            return Err(LinkerError::LibraryNotFound {
                name: name.clone(),
                raw: format!("{name} => not found"),
            });
        }
        Ok(listing.resolved)
    }

    /// Like [`Self::trace_dependencies`], but keeps libraries that failed to resolve.
    ///
    /// GNU ld.so exits non-zero when any dependency is missing and still prints the
    /// resolved entries on stdout. Treating that as a hard command failure dropped
    /// private-directory libraries such as `libpulsecommon-*.so` (matrix: MISSING_SO).
    pub fn trace_dependencies_partial(
        &self,
        linker: &Path,
        subject: &Path,
        search_paths: &[PathBuf],
    ) -> Result<TraceListing, LinkerError> {
        let subject = Self::resolve_subject(subject);
        let cache_key = CacheKey {
            linker: linker.to_path_buf(),
            subject: subject.clone(),
            search_paths: search_paths.to_vec(),
        };

        if let Some(cached) = self
            .cache
            .lock()
            .ok()
            .and_then(|map| map.get(&cache_key).cloned())
        {
            return Ok(cached);
        }

        let mut command = Command::new(linker);
        command.arg("--list").arg(&subject);
        command.env("LD_TRACE_LOADED_OBJECTS", "1");
        command.env("LC_ALL", "C");

        if !search_paths.is_empty() {
            let joined = join_paths(search_paths)?;
            command.env("LD_LIBRARY_PATH", joined);
        }

        let output = command.output().map_err(|source| LinkerError::Spawn {
            linker: linker.to_path_buf(),
            source,
        })?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if is_gcompat_stub(&stdout, &stderr) {
            return Err(LinkerError::UnsupportedStub {
                linker: linker.to_path_buf(),
                message: format_stub_message(&stdout, &stderr),
            });
        }

        let parsed = match interpret_linker_output(output.status.success(), &stdout) {
            Ok(parsed) => parsed,
            Err(()) => {
                let detail = if stderr.is_empty() {
                    stdout.trim().to_string()
                } else {
                    stderr
                };
                return Err(LinkerError::CommandFailed {
                    linker: linker.to_path_buf(),
                    status: output.status.code(),
                    stderr: detail,
                });
            }
        };

        if let Ok(mut map) = self.cache.lock() {
            map.insert(cache_key, parsed.clone());
        }

        Ok(parsed)
    }
}

pub fn is_gcompat_stub_binary(path: &Path) -> Result<bool, std::io::Error> {
    // gcompat stub banner appears near the middle of the file; grab a generous slice.
    const MAX_BYTES: usize = 64 * 1024;
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; MAX_BYTES];
    let read = std::io::Read::read(&mut file, &mut buf)?;
    buf.truncate(read);
    let haystack = String::from_utf8_lossy(&buf);
    Ok(haystack.contains("gcompat ELF interpreter stub"))
}

fn join_paths(paths: &[PathBuf]) -> Result<String, LinkerError> {
    let mut encoded = Vec::new();
    for path in paths {
        let as_str = path
            .to_str()
            .ok_or_else(|| LinkerError::InvalidPath(path.clone()))?;
        encoded.push(as_str.to_string());
    }
    Ok(encoded.join(":"))
}

/// Keep a partial listing when ld.so exits non-zero after printing dependencies.
/// An empty failed run is still a command error.
fn interpret_linker_output(status_ok: bool, stdout: &str) -> Result<TraceListing, ()> {
    let parsed = parse_trace_output(stdout);
    if !status_ok && parsed.resolved.is_empty() && parsed.missing.is_empty() {
        Err(())
    } else {
        Ok(parsed)
    }
}

fn parse_trace_output(output: &str) -> TraceListing {
    let mut resolved = Vec::new();
    let mut missing = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.contains("statically linked") {
            continue;
        }

        if let Some((name, rest)) = trimmed.split_once("=>") {
            let lib_name = name.trim().to_string();
            if lib_name.is_empty() {
                continue;
            }
            let rhs = rest.trim();
            if rhs.starts_with("not found") {
                missing.push(lib_name);
                continue;
            }
            let path_part = rhs.split_whitespace().next().unwrap_or("");
            if path_part.starts_with('/') {
                resolved.push(LibraryResolution {
                    name: lib_name,
                    target: PathBuf::from(path_part),
                });
            }
            continue;
        }

        if trimmed.starts_with('/') {
            let path_part = trimmed.split_whitespace().next().unwrap_or("");
            resolved.push(LibraryResolution {
                name: path_part.to_string(),
                target: PathBuf::from(path_part),
            });
        }
    }

    TraceListing { resolved, missing }
}

fn is_gcompat_stub(stdout: &str, stderr: &str) -> bool {
    let needle = "gcompat ELF interpreter stub";
    stdout.contains(needle) || stderr.contains(needle)
}

fn format_stub_message(stdout: &str, stderr: &str) -> String {
    if stdout.is_empty() && stderr.is_empty() {
        return "linker reported unsupported stub (gcompat?)".to_string();
    }
    let mut parts = Vec::new();
    if !stdout.is_empty() {
        parts.push(format!("stdout: {stdout}"));
    }
    if !stderr.is_empty() {
        parts.push(format!("stderr: {stderr}"));
    }
    parts.join(" | ")
}

#[derive(Debug, Error)]
pub enum LinkerError {
    #[error("failed to spawn linker {linker}: {source}")]
    Spawn {
        linker: PathBuf,
        source: std::io::Error,
    },
    #[error("linker {linker} exited with {status:?}: {stderr}")]
    CommandFailed {
        linker: PathBuf,
        status: Option<i32>,
        stderr: String,
    },
    #[error("linker reported missing library {name}: {raw}")]
    LibraryNotFound { name: String, raw: String },
    #[error("path contains invalid UTF-8: {0:?}")]
    InvalidPath(PathBuf),
    #[error("linker {linker} unsupported stub: {message}")]
    UnsupportedStub { linker: PathBuf, message: String },
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct CacheKey {
    linker: PathBuf,
    subject: PathBuf,
    search_paths: Vec<PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::{is_gcompat_stub_binary, LinkerRunner};
    use std::fs;
    use std::path::PathBuf;
    use tempfile::tempdir;

    #[test]
    fn detects_gcompat_stub_by_contents() {
        // Hex blob for a tiny gcompat stub with the marker string embedded.
        // This is not a full ELF parser test; we only care that the content matcher works.
        const STUB_BYTES: &[u8] = b"This is the gcompat ELF interpreter stub.\0";
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("ld-linux-x86-64.so.2");
        fs::write(&path, STUB_BYTES).unwrap();
        assert!(is_gcompat_stub_binary(&path).unwrap());
    }

    #[test]
    fn non_stub_binaries_are_not_flagged() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("ld-linux-x86-64.so.2");
        fs::write(&path, b"\x7fELF\0\0\0\0").unwrap();
        assert!(!is_gcompat_stub_binary(&path).unwrap());
    }

    #[test]
    fn detects_real_gcompat_stub_fixture() {
        let fixture = PathBuf::from("tests/fixtures/ld-linux-x86-64.gcompat.so");
        if !fixture.exists() {
            panic!("fixture missing: {}", fixture.display());
        }
        assert!(is_gcompat_stub_binary(&fixture).unwrap());
    }

    #[test]
    fn parse_trace_keeps_resolved_and_missing_libraries() {
        let listing = super::parse_trace_output(
            "\tlinux-vdso.so.1 (0x1)\n\tlibpulse.so.0 => /usr/lib/libpulse.so.0 (0x2)\n\tlibpulsecommon-16.1.so => not found\n\t/lib64/ld-linux-x86-64.so.2 (0x3)\n",
        );
        assert_eq!(listing.resolved.len(), 2);
        assert_eq!(listing.resolved[0].name, "libpulse.so.0");
        assert_eq!(listing.missing, vec!["libpulsecommon-16.1.so".to_string()]);
    }

    #[test]
    fn nonzero_linker_status_keeps_partial_listing() {
        let listing = super::interpret_linker_output(
            false,
            "\tlibfoo.so => not found\n\tlibc.so.6 => /lib/libc.so.6 (0x1)\n",
        )
        .unwrap();
        assert_eq!(listing.missing, vec!["libfoo.so".to_string()]);
        assert_eq!(listing.resolved.len(), 1);
        assert!(super::interpret_linker_output(false, "boom\n").is_err());
    }

    #[test]
    fn resolve_subject_prefers_canonical_target() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("real");
        let link = dir.path().join("link");
        fs::write(&target, b"x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let resolved = LinkerRunner::resolve_subject(&link);
        assert_eq!(resolved, target);
    }
}
