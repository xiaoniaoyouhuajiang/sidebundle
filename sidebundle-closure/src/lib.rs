use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use log::debug;
use sidebundle_core::{
    parse_elf_metadata, BundleEntry, BundleSpec, DependencyClosure, ElfMetadata, ElfParseError,
    EntryBundlePlan, ResolvedFile,
};
use thiserror::Error;

const DEFAULT_LIBRARY_DIRS: &[&str] = &[
    "/lib",
    "/lib64",
    "/usr/lib",
    "/usr/lib64",
    "/usr/lib/x86_64-linux-gnu",
    "/usr/local/lib",
];

/// Builds dependency closures for host executables.
#[derive(Default)]
pub struct ClosureBuilder {
    ld_library_paths: Vec<PathBuf>,
    default_paths: Vec<PathBuf>,
}

impl ClosureBuilder {
    pub fn new() -> Self {
        Self {
            ld_library_paths: env::var("LD_LIBRARY_PATH")
                .ok()
                .map(|value| Self::split_paths(&value))
                .unwrap_or_default(),
            default_paths: DEFAULT_LIBRARY_DIRS
                .iter()
                .map(|dir| PathBuf::from(dir))
                .collect(),
        }
    }

    pub fn build(&self, spec: &BundleSpec) -> Result<DependencyClosure, ClosureError> {
        if spec.entries().is_empty() {
            return Ok(DependencyClosure::default());
        }

        let mut file_map: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();
        let mut entry_plans = Vec::new();
        let mut elf_cache: HashMap<PathBuf, ElfMetadata> = HashMap::new();

        for entry in spec.entries() {
            let plan = self.build_entry(entry, &mut file_map, &mut elf_cache)?;
            entry_plans.push(plan);
        }

        let files = file_map
            .into_iter()
            .map(|(source, destination)| ResolvedFile { source, destination })
            .collect();

        Ok(DependencyClosure { files, entry_plans })
    }

    fn build_entry(
        &self,
        entry: &BundleEntry,
        files: &mut BTreeMap<PathBuf, PathBuf>,
        cache: &mut HashMap<PathBuf, ElfMetadata>,
    ) -> Result<EntryBundlePlan, ClosureError> {
        let entry_source = canonicalize(&entry.path)?;
        let entry_metadata = self.load_metadata(&entry_source, cache)?;
        let entry_dest = ensure_file(files, &entry_source);

        let interpreter_path = entry_metadata
            .interpreter
            .clone()
            .ok_or_else(|| ClosureError::MissingInterpreter {
                path: entry_source.clone(),
            })?;
        let interpreter_source = canonicalize(&interpreter_path)?;
        let interpreter_dest = ensure_file(files, &interpreter_source);

        let mut lib_dirs: BTreeSet<PathBuf> = BTreeSet::new();
        if let Some(dir) = entry_dest.parent() {
            lib_dirs.insert(dir.to_path_buf());
        }

        let mut visited: HashSet<PathBuf> = HashSet::new();
        let mut queue: VecDeque<PathBuf> = VecDeque::new();
        queue.push_back(entry_source.clone());

        while let Some(current) = queue.pop_front() {
            if !visited.insert(current.clone()) {
                continue;
            }

            let metadata = self.load_metadata(&current, cache)?;
            let search_paths = self.compute_search_paths(&current, &metadata);

            for lib_name in metadata.needed.iter() {
                if Self::should_skip(lib_name) {
                    continue;
                }

                let lib_path = self
                    .locate_library(lib_name, &search_paths)
                    .ok_or_else(|| ClosureError::LibraryNotFound {
                        name: lib_name.to_string(),
                        needed_by: current.clone(),
                    })?;

                let canonical = canonicalize(&lib_path)?;
                let dest = ensure_file(files, &canonical);
                if let Some(dir) = dest.parent() {
                    lib_dirs.insert(dir.to_path_buf());
                }
                queue.push_back(canonical);
            }
        }

        Ok(EntryBundlePlan {
            display_name: entry.display_name.clone(),
            binary_source: entry_source.clone(),
            binary_destination: entry_dest,
            linker_source: interpreter_source,
            linker_destination: interpreter_dest,
            library_dirs: lib_dirs.into_iter().collect(),
        })
    }

    fn load_metadata<'a>(
        &self,
        path: &Path,
        cache: &'a mut HashMap<PathBuf, ElfMetadata>,
    ) -> Result<&'a ElfMetadata, ClosureError> {
        if !cache.contains_key(path) {
            let metadata = parse_elf_metadata(path).map_err(|source| ClosureError::ElfParse {
                path: path.to_path_buf(),
                source,
            })?;
            cache.insert(path.to_path_buf(), metadata);
        }
        Ok(cache.get(path).expect("metadata cached"))
    }

    fn compute_search_paths(&self, binary: &Path, metadata: &ElfMetadata) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        let origin = binary.parent().unwrap_or_else(|| Path::new("/"));
        let preferred = if metadata.runpaths.is_empty() {
            &metadata.rpaths
        } else {
            &metadata.runpaths
        };

        for segment in preferred {
            if let Some(path) = Self::expand_origin(segment, origin) {
                paths.push(path);
            }
        }

        paths.extend(self.ld_library_paths.clone());
        paths.push(origin.to_path_buf());
        paths.extend(self.default_paths.clone());
        paths
    }

    fn locate_library(&self, name: &str, search_paths: &[PathBuf]) -> Option<PathBuf> {
        let candidate = Path::new(name);
        if candidate.is_absolute() && candidate.exists() {
            return Some(candidate.to_path_buf());
        }

        for dir in search_paths {
            let path = dir.join(name);
            if path.exists() {
                debug!("Located {} at {}", name, path.display());
                return Some(path);
            }
        }
        None
    }

    fn expand_origin(segment: &str, origin: &Path) -> Option<PathBuf> {
        if segment.trim().is_empty() {
            return None;
        }
        let origin_str = origin.to_str().unwrap_or(".");
        let replaced = segment
            .replace("$ORIGIN", origin_str)
            .replace("${ORIGIN}", origin_str);
        if replaced.is_empty() {
            return None;
        }
        let candidate = PathBuf::from(&replaced);
        if candidate.is_absolute() {
            Some(candidate)
        } else {
            Some(origin.join(candidate))
        }
    }

    fn should_skip(name: &str) -> bool {
        name.starts_with("linux-vdso") || name.starts_with("ld-linux")
    }

    fn split_paths(value: &str) -> Vec<PathBuf> {
        value
            .split(':')
            .filter(|segment| !segment.trim().is_empty())
            .map(PathBuf::from)
            .collect()
    }
}

fn canonicalize(path: &Path) -> Result<PathBuf, ClosureError> {
    fs::canonicalize(path).map_err(|source| ClosureError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn ensure_file(mapping: &mut BTreeMap<PathBuf, PathBuf>, source: &Path) -> PathBuf {
    if let Some(dest) = mapping.get(source) {
        return dest.clone();
    }
    let mut dest = PathBuf::from("payload");
    if source.is_absolute() {
        for component in source.components().skip(1) {
            dest.push(component.as_os_str());
        }
    } else {
        dest.push(source);
    }
    mapping.insert(source.to_path_buf(), dest.clone());
    dest
}

#[derive(Debug, Error)]
pub enum ClosureError {
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("ELF parse error {path}: {source}")]
    ElfParse {
        path: PathBuf,
        source: ElfParseError,
    },
    #[error("binary {path} lacks PT_INTERP linker")]
    MissingInterpreter {
        path: PathBuf,
    },
    #[error("failed to locate {name}, required by {needed_by}")]
    LibraryNotFound {
        name: String,
        needed_by: PathBuf,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use sidebundle_core::{BundleEntry, TargetTriple};

    #[test]
    fn closure_collects_host_binary() {
        #[cfg(target_os = "linux")]
        {
            let spec = BundleSpec::new("demo", TargetTriple::linux_x86_64())
                .with_entry(BundleEntry::new("/bin/ls", "ls"));
            let closure = ClosureBuilder::new().build(&spec).unwrap();
            assert!(
                !closure.files.is_empty(),
                "expected /bin/ls closure to contain files"
            );
            assert!(
                closure.entry_plans.iter().any(|plan| plan.display_name == "ls"),
                "entry plan should include launcher info"
            );
        }
    }
}
