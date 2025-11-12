use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use nix::unistd::{chdir, chroot};
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct LibraryResolution {
    pub name: String,
    pub target: PathBuf,
}

#[derive(Debug, Clone)]
pub struct LinkerRunner {
    root: Option<PathBuf>,
    cache: Arc<Mutex<HashMap<CacheKey, Vec<LibraryResolution>>>>,
}

impl Default for LinkerRunner {
    fn default() -> Self {
        Self {
            root: None,
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl LinkerRunner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.root = Some(root.into());
        self
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
            root: self.root.clone(),
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

        let exec_linker = self.translate_exec_path(linker)?;
        let exec_subject = self.translate_exec_path(subject)?;

        let mut command = Command::new(&exec_linker);
        command.arg("--list").arg(&exec_subject);
        command.env("LD_TRACE_LOADED_OBJECTS", "1");
        command.env("LC_ALL", "C");

        if !search_paths.is_empty() {
            let translated_paths = self.translate_paths(search_paths)?;
            let joined = join_paths(&translated_paths)?;
            command.env("LD_LIBRARY_PATH", joined);
        }

        if let Some(root) = &self.root {
            let root_clone = root.clone();
            unsafe {
                command.pre_exec(move || {
                    chdir(&root_clone).map_err(nix_err_to_io)?;
                    chroot(".").map_err(nix_err_to_io)?;
                    chdir(Path::new("/")).map_err(nix_err_to_io)?;
                    Ok(())
                });
            }
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
    #[error("path {path} is outside of chroot root {root}")]
    PathOutsideRoot { path: PathBuf, root: PathBuf },
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct CacheKey {
    linker: PathBuf,
    subject: PathBuf,
    root: Option<PathBuf>,
    search_paths: Vec<PathBuf>,
}

impl LinkerRunner {
    fn translate_exec_path(&self, path: &Path) -> Result<PathBuf, LinkerError> {
        if let Some(root) = &self.root {
            translate_within_root(root, path)
        } else {
            Ok(path.to_path_buf())
        }
    }

    fn translate_paths(&self, paths: &[PathBuf]) -> Result<Vec<PathBuf>, LinkerError> {
        paths
            .iter()
            .map(|path| self.translate_exec_path(path))
            .collect()
    }
}

fn translate_within_root(root: &Path, path: &Path) -> Result<PathBuf, LinkerError> {
    if let Ok(rel) = path.strip_prefix(root) {
        let mut translated = PathBuf::from("/");
        translated.push(rel);
        Ok(translated)
    } else if path.is_absolute() {
        let rel = path.strip_prefix("/").unwrap_or(path);
        let rebased = root.join(rel);
        if fs::metadata(&rebased).is_ok() {
            Ok(rebased)
        } else {
            Err(LinkerError::PathOutsideRoot {
                path: path.to_path_buf(),
                root: root.to_path_buf(),
            })
        }
    } else {
        Err(LinkerError::PathOutsideRoot {
            path: path.to_path_buf(),
            root: root.to_path_buf(),
        })
    }
}

fn nix_err_to_io(err: nix::Error) -> io::Error {
    io::Error::from(err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn translate_path_inside_root() {
        let root = PathBuf::from("/tmp/rootfs");
        let runner = LinkerRunner::new().with_root(&root);
        let target = runner
            .translate_exec_path(Path::new("/tmp/rootfs/usr/bin/ld-linux.so"))
            .expect("translate success");
        assert_eq!(target, PathBuf::from("/usr/bin/ld-linux.so"));
    }

    #[test]
    fn translate_path_outside_root_errors() {
        let root = PathBuf::from("/tmp/rootfs");
        let runner = LinkerRunner::new().with_root(&root);
        let err = runner
            .translate_exec_path(Path::new("/usr/bin/ld"))
            .expect_err("outside root");
        match err {
            LinkerError::PathOutsideRoot { .. } => {}
            other => panic!("unexpected error {other:?}"),
        }
    }
}
