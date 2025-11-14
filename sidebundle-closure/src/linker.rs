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

#[derive(Debug, Clone)]
pub struct LinkerRunner {
    cache: Arc<Mutex<HashMap<CacheKey, Vec<LibraryResolution>>>>,
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

    pub fn trace_dependencies(
        &self,
        linker: &Path,
        subject: &Path,
        search_paths: &[PathBuf],
    ) -> Result<Vec<LibraryResolution>, LinkerError> {
        let cache_key = CacheKey {
            linker: linker.to_path_buf(),
            subject: subject.to_path_buf(),
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
        command.arg("--list").arg(subject);
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

        if !output.status.success() {
            return Err(LinkerError::CommandFailed {
                linker: linker.to_path_buf(),
                status: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let parsed = parse_trace_output(stdout.as_ref())?;

        if let Ok(mut map) = self.cache.lock() {
            map.insert(cache_key, parsed.clone());
        }

        Ok(parsed)
    }
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

fn parse_trace_output(output: &str) -> Result<Vec<LibraryResolution>, LinkerError> {
    let mut libs = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.contains("statically linked") {
            continue;
        }

        if let Some((name, rest)) = trimmed.split_once("=>") {
            let lib_name = name.trim().to_string();
            let rhs = rest.trim();
            if rhs.starts_with("not found") {
                return Err(LinkerError::LibraryNotFound {
                    name: lib_name,
                    raw: trimmed.to_string(),
                });
            }
            let path_part = rhs.split_whitespace().next().unwrap_or("");
            if path_part.starts_with('/') {
                libs.push(LibraryResolution {
                    name: lib_name,
                    target: PathBuf::from(path_part),
                });
            }
            continue;
        }

        if trimmed.starts_with('/') {
            let path_part = trimmed.split_whitespace().next().unwrap_or("");
            libs.push(LibraryResolution {
                name: path_part.to_string(),
                target: PathBuf::from(path_part),
            });
        }
    }

    Ok(libs)
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
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct CacheKey {
    linker: PathBuf,
    subject: PathBuf,
    search_paths: Vec<PathBuf>,
}

#[cfg(test)]
mod tests {}
