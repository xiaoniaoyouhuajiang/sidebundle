mod linker;
pub mod trace;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use linker::{LibraryResolution, LinkerError, LinkerRunner};
use log::debug;
use sha2::{Digest, Sha256};
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
    runner: LinkerRunner,
    tracer: Option<trace::TraceCollector>,
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
            runner: LinkerRunner::new(),
            tracer: None,
        }
    }

    pub fn with_chroot_root(mut self, root: impl Into<PathBuf>) -> Self {
        let root_buf = root.into();
        self.runner = self.runner.clone().with_root(root_buf.clone());
        if let Some(tracer) = self.tracer.take() {
            self.tracer = Some(tracer.with_root(root_buf.clone()));
        }
        self
    }

    pub fn with_tracer(mut self, tracer: trace::TraceCollector) -> Self {
        self.tracer = Some(tracer);
        self
    }

    pub fn build(&self, spec: &BundleSpec) -> Result<DependencyClosure, ClosureError> {
        if spec.entries().is_empty() {
            return Ok(DependencyClosure::default());
        }

        let mut file_map: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();
        let mut entry_plans = Vec::new();
        let mut traced_files = BTreeSet::new();
        let mut elf_cache: HashMap<PathBuf, ElfMetadata> = HashMap::new();

        for entry in spec.entries() {
            let plan = self.build_entry(entry, &mut file_map, &mut elf_cache)?;
            entry_plans.push(plan);
            if let Some(tracer) = &self.tracer {
                match tracer.run(&[entry.path.to_string_lossy().to_string()]) {
                    Ok(report) => traced_files.extend(report.files),
                    Err(err) => debug!("trace for `{}` failed: {err}", entry.path.display()),
                }
            }
        }

        let mut files = Vec::new();
        for (source, destination) in file_map.into_iter() {
            let digest = compute_digest(&source)?;
            files.push(ResolvedFile {
                source,
                destination,
                digest,
            });
        }

        Ok(DependencyClosure {
            files,
            entry_plans,
            traced_files: traced_files.into_iter().collect(),
        })
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

        let (interpreter_source, interpreter_dest, is_static) =
            match entry_metadata.interpreter.clone() {
                Some(path) => {
                    let canonical = canonicalize(&path)?;
                    let dest = ensure_file(files, &canonical);
                    (Some(canonical), Some(dest), false)
                }
                None => (None, None, true),
            };

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
            if is_static {
                continue;
            }
            let interpreter = interpreter_source.as_ref().expect("static skipped");
            let search_paths = self.compute_search_paths(&current, &metadata);
            let resolved =
                self.trace_with_linker(interpreter, &current, &search_paths, metadata)?;

            for resolution in resolved {
                if Self::should_skip(&resolution.name) {
                    continue;
                }

                let canonical = canonicalize(&resolution.target)?;
                let dest = ensure_file(files, &canonical);
                if let Some(dir) = dest.parent() {
                    lib_dirs.insert(dir.to_path_buf());
                }
                queue.push_back(canonical);
            }
        }

        let libraries: Vec<PathBuf> = lib_dirs.into_iter().collect();
        let binary_destination = entry_dest.clone();
        Ok(EntryBundlePlan {
            display_name: entry.display_name.clone(),
            binary_source: entry_source.clone(),
            binary_destination: binary_destination.clone(),
            linker_source: interpreter_source.unwrap_or_else(|| entry_source.clone()),
            linker_destination: interpreter_dest.unwrap_or_else(|| binary_destination.clone()),
            library_dirs: libraries,
            requires_linker: !is_static,
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

    fn trace_with_linker(
        &self,
        linker: &Path,
        subject: &Path,
        search_paths: &[PathBuf],
        metadata: &ElfMetadata,
    ) -> Result<Vec<LibraryResolution>, ClosureError> {
        if metadata.needed.is_empty() {
            return Ok(Vec::new());
        }
        self.runner
            .trace_dependencies(linker, subject, search_paths)
            .map_err(|source| ClosureError::LinkerTrace {
                path: subject.to_path_buf(),
                source,
            })
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

fn compute_digest(path: &Path) -> Result<String, ClosureError> {
    let mut file = fs::File::open(path).map_err(|source| ClosureError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = file.read(&mut buffer).map_err(|source| ClosureError::Io {
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
        use std::fmt::Write;
        write!(&mut hex, "{:02x}", byte).expect("write to string");
    }
    Ok(hex)
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
    MissingInterpreter { path: PathBuf },
    #[error("linker trace failed for {path}: {source}")]
    LinkerTrace { path: PathBuf, source: LinkerError },
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
                closure
                    .entry_plans
                    .iter()
                    .any(|plan| plan.display_name == "ls"),
                "entry plan should include launcher info"
            );
        }
    }
}
