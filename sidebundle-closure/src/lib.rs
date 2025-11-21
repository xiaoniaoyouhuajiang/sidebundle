pub mod image;
mod linker;
pub mod trace;
pub mod validator;

use crate::image::ImageRoot;
use log::warn;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use linker::{is_gcompat_stub_binary, LibraryResolution, LinkerError, LinkerRunner};
use log::debug;
use sha2::{Digest, Sha256};
use shell_words;
use sidebundle_core::{
    parse_elf_metadata, BinaryEntryPlan, BundleEntry, BundleSpec, DependencyClosure, ElfMetadata,
    ElfParseError, EntryBundlePlan, LogicalPath, Origin, ResolvedFile, ScriptEntryPlan, TracedFile,
};
use thiserror::Error;
use regex::Regex;
const DEFAULT_LIBRARY_DIRS: &[&str] = &[
    "/lib",
    "/lib64",
    "/usr/lib",
    "/usr/lib64",
    "/usr/lib/x86_64-linux-gnu",
    "/usr/local/lib",
];

const DEFAULT_SHEBANG_PATHS: &[&str] = &[
    "/usr/local/sbin",
    "/usr/local/bin",
    "/usr/sbin",
    "/usr/bin",
    "/sbin",
    "/bin",
];

const TRACE_SKIP_PREFIXES: &[&str] = &["/proc", "/sys", "/dev", "/run", "/var/run"];
const TRACE_SKIP_FILENAMES: &[&str] = &["locale-archive"];
const GLIBC_HWCAPS_SEGMENT: &str = "glibc-hwcaps";
const GPU_LIB_PREFIXES: &[&str] = &[
    "libgl",
    "libgldispatch",
    "libglx",
    "libegl",
    "libgles",
    "libopencl",
    "libvulkan",
    "libcuda",
    "libnvidia",
    "libdrm",
    "libnvoptix",
    "libopenvr",
    "libxnvctrl",
];

const HOST_GLIBC_LINKERS: &[&str] = &[
    "/lib64/ld-linux-x86-64.so.2",
    "/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2",
    "/lib/ld-linux-x86-64.so.2",
];

struct ShebangSpec {
    interpreter_host: PathBuf,
    args: Vec<String>,
}

pub trait PathResolver: Send + Sync {
    fn trace_root(&self) -> Option<&Path>;
    fn to_host(&self, logical: &LogicalPath) -> PathBuf;
    fn to_trace_path(&self, logical: &LogicalPath) -> PathBuf;
    fn runtime_to_host(&self, traced: &Path) -> Option<PathBuf>;
    fn host_to_logical(&self, host_path: &Path) -> Option<LogicalPath>;
}

#[derive(Default)]
pub struct HostPathResolver;

impl PathResolver for HostPathResolver {
    fn trace_root(&self) -> Option<&Path> {
        None
    }

    fn to_host(&self, logical: &LogicalPath) -> PathBuf {
        logical.path().to_path_buf()
    }

    fn to_trace_path(&self, logical: &LogicalPath) -> PathBuf {
        logical.path().to_path_buf()
    }

    fn runtime_to_host(&self, traced: &Path) -> Option<PathBuf> {
        Some(traced.to_path_buf())
    }

    fn host_to_logical(&self, host_path: &Path) -> Option<LogicalPath> {
        Some(LogicalPath::new(Origin::Host, host_path.to_path_buf()))
    }
}

pub struct ChrootPathResolver {
    root: PathBuf,
    origin: Origin,
    #[allow(dead_code)]
    guard: Option<ImageRoot>,
}

impl ChrootPathResolver {
    pub fn from_root(root: PathBuf, origin: Origin) -> Self {
        Self {
            root,
            origin,
            guard: None,
        }
    }

    pub fn from_image(image: ImageRoot, origin: Origin) -> Self {
        let root = image.rootfs().to_path_buf();
        Self {
            root,
            origin,
            guard: Some(image),
        }
    }
}

impl PathResolver for ChrootPathResolver {
    fn trace_root(&self) -> Option<&Path> {
        Some(&self.root)
    }

    fn to_host(&self, logical: &LogicalPath) -> PathBuf {
        let path = logical.path();
        if path.is_absolute() {
            let stripped = path.strip_prefix("/").unwrap_or(path);
            self.root.join(stripped)
        } else {
            self.root.join(path)
        }
    }

    fn to_trace_path(&self, logical: &LogicalPath) -> PathBuf {
        let path = logical.path();
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            let mut rebuilt = PathBuf::from("/");
            rebuilt.push(path);
            rebuilt
        }
    }

    fn runtime_to_host(&self, traced: &Path) -> Option<PathBuf> {
        if traced.is_absolute() {
            let stripped = traced.strip_prefix("/").unwrap_or(traced);
            Some(self.root.join(stripped))
        } else {
            Some(self.root.join(traced))
        }
    }

    fn host_to_logical(&self, host_path: &Path) -> Option<LogicalPath> {
        let rel = host_path.strip_prefix(&self.root).ok()?;
        let mut rebuilt = PathBuf::from("/");
        rebuilt.push(rel);
        Some(LogicalPath::new(self.origin.clone(), rebuilt))
    }
}

#[derive(Clone)]
pub struct ResolverSet {
    resolvers: HashMap<Origin, Arc<dyn PathResolver>>,
}

impl ResolverSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, origin: Origin, resolver: Arc<dyn PathResolver>) {
        self.resolvers.insert(origin, resolver);
    }

    pub fn get(&self, origin: &Origin) -> Option<Arc<dyn PathResolver>> {
        self.resolvers.get(origin).cloned()
    }
}

impl Default for ResolverSet {
    fn default() -> Self {
        let mut map: HashMap<Origin, Arc<dyn PathResolver>> = HashMap::new();
        map.insert(Origin::Host, Arc::new(HostPathResolver::default()));
        Self { resolvers: map }
    }
}

/// Builds dependency closures for host executables.
pub struct ClosureBuilder {
    ld_library_paths: Vec<PathBuf>,
    default_paths: Vec<PathBuf>,
    origin_paths: HashMap<Origin, Vec<PathBuf>>,
    runner: LinkerRunner,
    tracer: Option<trace::TraceCollector>,
    resolvers: ResolverSet,
    external_traces: HashMap<Origin, Vec<PathBuf>>,
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
            origin_paths: HashMap::new(),
            runner: LinkerRunner::new(),
            tracer: None,
            resolvers: ResolverSet::new(),
            external_traces: HashMap::new(),
        }
    }

    pub fn with_resolver_set(mut self, resolvers: ResolverSet) -> Self {
        self.resolvers = resolvers;
        self
    }

    pub fn with_resolver(mut self, origin: Origin, resolver: Arc<dyn PathResolver>) -> Self {
        self.resolvers.insert(origin, resolver);
        self
    }

    pub fn with_tracer(mut self, tracer: trace::TraceCollector) -> Self {
        self.tracer = Some(tracer);
        self
    }

    pub fn with_origin_path(mut self, origin: Origin, path: Vec<PathBuf>) -> Self {
        self.origin_paths.insert(origin, path);
        self
    }

    pub fn with_external_trace_paths(mut self, origin: Origin, paths: Vec<PathBuf>) -> Self {
        self.external_traces
            .entry(origin)
            .or_insert_with(Vec::new)
            .extend(paths);
        self
    }

    pub fn build(&mut self, spec: &BundleSpec) -> Result<DependencyClosure, ClosureError> {
        if spec.entries().is_empty() {
            return Ok(DependencyClosure::default());
        }

        let mut runtime_aliases: HashMap<PathBuf, BTreeSet<PathBuf>> = HashMap::new();

        let mut file_map: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();
        let mut entry_plans = Vec::new();
        let mut traced_map: HashMap<Origin, BTreeMap<PathBuf, TracedFile>> = HashMap::new();
        let mut elf_cache: HashMap<PathBuf, ElfMetadata> = HashMap::new();
        let mut traced_files_acc: Vec<TracedFile> = Vec::new();

        for entry in spec.entries() {
            let resolver = self.resolver_for(entry.logical.origin())?;
            let plan = self.build_entry(
                entry,
                resolver.as_ref(),
                &mut file_map,
                &mut runtime_aliases,
                &mut elf_cache,
            )?;
            if let Some(tracer) = &self.tracer {
                if let Some(command) = self.trace_command_for(entry, &plan, resolver.as_ref()) {
                    match tracer.run(resolver.as_ref(), &command) {
                        Ok(artifacts) => {
                            let origin = plan.origin().clone();
                            let origin_map = traced_map.entry(origin).or_default();
                            for record in artifacts {
                                if let Some(artifact) = self.make_trace_artifact(&record) {
                                    origin_map
                                        .entry(artifact.resolved.clone())
                                        .or_insert(artifact);
                                }
                            }
                        }
                        Err(err) => debug!(
                            "trace for `{}` failed: {err}",
                            entry.logical.path().display()
                        ),
                    }
                }
            }
            entry_plans.push(plan);
        }

        for (origin, runtime_paths) in &self.external_traces {
            let resolver = self.resolver_for(origin)?;
            let origin_map = traced_map.entry(origin.clone()).or_default();
            for runtime_path in runtime_paths {
                if let Some(host_path) = resolver.runtime_to_host(runtime_path) {
                    let canonical_host = match canonicalize(&host_path, resolver.trace_root()) {
                        Ok(path) => path,
                        Err(err) => {
                            warn!(
                                "skipping traced path {}: failed to canonicalize ({err})",
                                runtime_path.display()
                            );
                            continue;
                        }
                    };
                    let artifact = trace::TraceArtifact {
                        runtime_path: runtime_path.clone(),
                        host_path: Some(canonical_host.clone()),
                        logical_path: Some(LogicalPath::new(origin.clone(), runtime_path.clone())),
                    };
                    if let Some(traced) = self.make_trace_artifact(&artifact) {
                        origin_map.entry(traced.resolved.clone()).or_insert(traced);
                    }
                }
            }
        }

        for (origin, artifacts) in traced_map {
            let resolver = self.resolver_for(&origin)?;
            let traced_files: Vec<TracedFile> = artifacts.values().cloned().collect();
            self.promote_traced_elves(
                resolver.as_ref(),
                &origin,
                &mut file_map,
                &mut runtime_aliases,
                &mut elf_cache,
                &traced_files,
            )?;
            self.promote_traced_resources(
                resolver.as_ref(),
                &mut file_map,
                &mut runtime_aliases,
                &traced_files,
            );
            traced_files_acc.extend(traced_files);
        }

        let mut files = Vec::new();
        let mut seen_destinations: HashSet<PathBuf> = HashSet::new();
        for (source, destination) in file_map.into_iter() {
            if let Some(reason) = classify_high_risk_asset(&source) {
                debug!(
                    "omitting {} from closure (filtered: {reason})",
                    source.display()
                );
                continue;
            }
            if !seen_destinations.insert(destination.clone()) {
                debug!(
                    "omitting {} from closure (destination {} already populated)",
                    source.display(),
                    destination.display()
                );
                continue;
            }
            let digest = compute_digest(&source)?;
            files.push(ResolvedFile {
                source,
                destination,
                digest,
            });
        }

        let runtime_aliases = runtime_aliases
            .iter()
            .map(|(source, aliases)| (source.clone(), aliases.iter().cloned().collect()))
            .collect();

        Ok(DependencyClosure {
            files,
            entry_plans,
            traced_files: traced_files_acc,
            runtime_aliases,
            symlinks: Vec::new(),
            metadata: HashMap::new(),
        })
    }

    fn resolver_for(&self, origin: &Origin) -> Result<Arc<dyn PathResolver>, ClosureError> {
        self.resolvers
            .get(origin)
            .ok_or_else(|| ClosureError::MissingResolver {
                origin: origin.clone(),
            })
    }

    fn canonical_entry_path(
        &self,
        resolver: &dyn PathResolver,
        logical: &LogicalPath,
    ) -> Result<PathBuf, ClosureError> {
        canonicalize(&resolver.to_host(logical), resolver.trace_root())
    }

    fn make_trace_artifact(&self, original: &trace::TraceArtifact) -> Option<TracedFile> {
        let resolved = original.host_path.as_ref()?;
        if !trace_path_allowed(resolved) {
            return None;
        }
        if let Some(reason) = classify_high_risk_asset(resolved) {
            debug!(
                "skipping traced artifact {} (filtered: {reason})",
                resolved.display()
            );
            return None;
        }
        let host_path = resolved.clone();
        let is_elf = parse_elf_metadata(&host_path).is_ok();
        Some(TracedFile {
            original: original.runtime_path.clone(),
            resolved: host_path,
            is_elf,
        })
    }

    fn promote_traced_elves(
        &self,
        resolver: &dyn PathResolver,
        origin: &Origin,
        files: &mut BTreeMap<PathBuf, PathBuf>,
        aliases: &mut HashMap<PathBuf, BTreeSet<PathBuf>>,
        cache: &mut HashMap<PathBuf, ElfMetadata>,
        traced: &[TracedFile],
    ) -> Result<(), ClosureError> {
        let mut promoted: HashSet<PathBuf> = HashSet::new();
        for artifact in traced {
            if !artifact.is_elf {
                continue;
            }
            if !promoted.insert(artifact.resolved.clone()) {
                continue;
            }
            let display = artifact
                .original
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("traced-entry");
            let _ = self.build_entry_plan(
                resolver,
                origin,
                &artifact.resolved,
                display,
                files,
                aliases,
                cache,
            )?;
        }
        Ok(())
    }

    fn promote_traced_resources(
        &self,
        resolver: &dyn PathResolver,
        files: &mut BTreeMap<PathBuf, PathBuf>,
        aliases: &mut HashMap<PathBuf, BTreeSet<PathBuf>>,
        traced: &[TracedFile],
    ) {
        for artifact in traced {
            if artifact.is_elf {
                continue;
            }
            let source = &artifact.resolved;
            if !source.exists() {
                continue;
            }
            if let Some(reason) = classify_high_risk_asset(source) {
                debug!("skipping traced resource {} ({reason})", source.display());
                continue;
            }
            let _ = ensure_file(resolver, files, aliases, source, Some(&artifact.original));
        }
    }

    fn build_entry(
        &self,
        entry: &BundleEntry,
        resolver: &dyn PathResolver,
        files: &mut BTreeMap<PathBuf, PathBuf>,
        aliases: &mut HashMap<PathBuf, BTreeSet<PathBuf>>,
        cache: &mut HashMap<PathBuf, ElfMetadata>,
    ) -> Result<EntryBundlePlan, ClosureError> {
        let entry_source = self.canonical_entry_path(resolver, &entry.logical)?;
        self.build_entry_plan(
            resolver,
            entry.logical.origin(),
            &entry_source,
            &entry.display_name,
            files,
            aliases,
            cache,
        )
    }

    fn build_entry_plan(
        &self,
        resolver: &dyn PathResolver,
        origin: &Origin,
        entry_source: &Path,
        display_name: &str,
        files: &mut BTreeMap<PathBuf, PathBuf>,
        aliases: &mut HashMap<PathBuf, BTreeSet<PathBuf>>,
        cache: &mut HashMap<PathBuf, ElfMetadata>,
    ) -> Result<EntryBundlePlan, ClosureError> {
        match parse_elf_metadata(entry_source) {
            Ok(metadata) => {
                cache
                    .entry(entry_source.to_path_buf())
                    .or_insert_with(|| metadata.clone());
                let plan = self.build_binary_plan(
                    resolver,
                    origin,
                    entry_source,
                    display_name,
                    files,
                    aliases,
                    cache,
                )?;
                Ok(EntryBundlePlan::Binary(plan))
            }
            Err(ElfParseError::NotElf { .. }) => {
                let shebang = self.parse_shebang(entry_source, resolver, origin)?;
                let interpreter_plan = self.build_binary_plan(
                    resolver,
                    origin,
                    &shebang.interpreter_host,
                    display_name,
                    files,
                    aliases,
                    cache,
                )?;
                let runtime_alias = resolver
                    .host_to_logical(entry_source)
                    .map(|logical| logical.path().to_path_buf());
                let script_destination = ensure_file(
                    resolver,
                    files,
                    aliases,
                    entry_source,
                    runtime_alias.as_deref(),
                );
                let plan = ScriptEntryPlan {
                    display_name: display_name.to_string(),
                    script_source: entry_source.to_path_buf(),
                    script_destination,
                    interpreter_source: interpreter_plan.binary_source.clone(),
                    interpreter_destination: interpreter_plan.binary_destination.clone(),
                    linker_source: interpreter_plan.linker_source.clone(),
                    linker_destination: interpreter_plan.linker_destination.clone(),
                    interpreter_args: shebang.args,
                    library_dirs: interpreter_plan.library_dirs.clone(),
                    requires_linker: interpreter_plan.requires_linker,
                    origin: origin.clone(),
                };
                if Self::is_bash_interpreter(&plan.interpreter_source) {
                    self.collect_bash_dependencies(
                        resolver,
                        origin,
                        entry_source,
                        files,
                        aliases,
                        cache,
                    );
                }
                Ok(EntryBundlePlan::Script(plan))
            }
            Err(source) => Err(ClosureError::ElfParse {
                path: entry_source.to_path_buf(),
                source,
            }),
        }
    }

    fn build_binary_plan(
        &self,
        resolver: &dyn PathResolver,
        origin: &Origin,
        entry_source: &Path,
        display_name: &str,
        files: &mut BTreeMap<PathBuf, PathBuf>,
        aliases: &mut HashMap<PathBuf, BTreeSet<PathBuf>>,
        cache: &mut HashMap<PathBuf, ElfMetadata>,
    ) -> Result<BinaryEntryPlan, ClosureError> {
        let entry_metadata = self.load_metadata(entry_source, cache)?;
        let entry_dest = ensure_file(resolver, files, aliases, entry_source, None);

        let (mut interpreter_source, mut interpreter_dest, is_static) =
            match entry_metadata.interpreter.clone() {
                Some(path) => {
                    let canonical = canonicalize(&path, resolver.trace_root())?;
                    let dest = ensure_file(resolver, files, aliases, &canonical, Some(&path));
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
        queue.push_back(entry_source.to_path_buf());

        while let Some(current) = queue.pop_front() {
            if !visited.insert(current.clone()) {
                continue;
            }

            let metadata = self.load_metadata(&current, cache)?;
            if is_static {
                continue;
            }
            let interpreter = interpreter_source.as_ref().expect("static skipped");
            let search_paths = self.compute_search_paths(resolver, origin, &current, &metadata);
            let resolved =
                self.trace_with_linker(interpreter, &current, &search_paths, metadata)?;

            for resolution in resolved {
                if Self::should_skip(&resolution.name) {
                    continue;
                }
                let canonical = canonicalize(&resolution.target, resolver.trace_root())?;
                if let Some(reason) = classify_high_risk_asset(&canonical) {
                    debug!(
                        "skipping dependency {} for {} ({reason})",
                        canonical.display(),
                        current.display()
                    );
                    continue;
                }
                let alias_runtime = resolver
                    .host_to_logical(&resolution.target)
                    .map(|logical| logical.path().to_path_buf());
                let dest = ensure_file(
                    resolver,
                    files,
                    aliases,
                    &canonical,
                    alias_runtime.as_deref(),
                );
                if let Some(dir) = dest.parent() {
                    lib_dirs.insert(dir.to_path_buf());
                    let mut alias_path = dir.to_path_buf();
                    alias_path.push(&resolution.name);
                    if let Some(logical) = resolver.host_to_logical(&canonical) {
                        let mut runtime_alias = logical.path().to_path_buf();
                        runtime_alias.set_file_name(&resolution.name);
                        record_alias(aliases, &canonical, &runtime_alias);
                    } else if alias_path != dest {
                        record_alias(aliases, &canonical, &alias_path);
                    }
                }
                queue.push_back(canonical);
            }
        }

        let libraries: Vec<PathBuf> = lib_dirs.into_iter().collect();
        let binary_destination = entry_dest.clone();
        let mut requires_linker = !is_static;
        if let Some(linker_path) = interpreter_source.clone() {
            match is_gcompat_stub_binary(&linker_path) {
                Ok(true) => {
                    if let Some(host_ld) = Self::find_host_glibc_linker() {
                        warn!(
                            "detected gcompat linker stub at {}; substituting host glibc linker {}",
                            linker_path.display(),
                            host_ld.display()
                        );
                        let stub_source = linker_path.clone();
                        let desired_dest = files.remove(&stub_source).unwrap_or_else(|| {
                            let mut dest = PathBuf::from("payload");
                            for comp in Path::new("/lib/ld-linux-x86-64.so.2").components().skip(1)
                            {
                                dest.push(comp.as_os_str());
                            }
                            dest
                        });
                        debug!(
                            "gcompat replacement: removing stub {} -> {}, inserting host {}",
                            stub_source.display(),
                            desired_dest.display(),
                            host_ld.display()
                        );
                        files.insert(host_ld.clone(), desired_dest.clone());
                        interpreter_source = Some(host_ld.clone());
                        interpreter_dest = Some(desired_dest);
                        debug!(
                            "gcompat replacement: interpreter_source={}, interpreter_dest={}",
                            host_ld.display(),
                            interpreter_dest.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "<none>".into())
                        );
                        requires_linker = true;
                    } else {
                        warn!(
                            "detected gcompat linker stub at {}; no host glibc linker found, launcher will exec binary directly (may fail)",
                            linker_path.display()
                        );
                        requires_linker = false;
                    }
                }
                Err(err) => warn!(
                    "failed to inspect linker {} for gcompat stub: {err}",
                    linker_path.display()
                ),
                _ => {}
            }
        }
        Ok(BinaryEntryPlan {
            display_name: display_name.to_string(),
            binary_source: entry_source.to_path_buf(),
            binary_destination: binary_destination.clone(),
            linker_source: interpreter_source.unwrap_or_else(|| entry_source.to_path_buf()),
            linker_destination: interpreter_dest.unwrap_or_else(|| binary_destination.clone()),
            library_dirs: libraries,
            requires_linker,
            origin: origin.clone(),
        })
    }

    fn parse_shebang(
        &self,
        script: &Path,
        resolver: &dyn PathResolver,
        origin: &Origin,
    ) -> Result<ShebangSpec, ClosureError> {
        let file = fs::File::open(script).map_err(|source| ClosureError::Io {
            path: script.to_path_buf(),
            source,
        })?;
        let mut reader = BufReader::new(file);
        let mut first_line = String::new();
        let _ = reader
            .read_line(&mut first_line)
            .map_err(|source| ClosureError::Io {
                path: script.to_path_buf(),
                source,
            })?;
        let trimmed = first_line.trim_end_matches(&['\r', '\n'][..]);
        if !trimmed.starts_with("#!") {
            return Err(ClosureError::UnsupportedEntry {
                path: script.to_path_buf(),
            });
        }
        let parts = shell_words::split(trimmed.trim_start_matches("#!").trim()).map_err(|err| {
            ClosureError::ShebangParse {
                path: script.to_path_buf(),
                reason: format!("failed to parse shebang tokens: {err}"),
            }
        })?;
        if parts.is_empty() {
            return Err(ClosureError::ShebangParse {
                path: script.to_path_buf(),
                reason: "shebang missing interpreter".into(),
            });
        }

        let mut iter = parts.into_iter();
        let raw_interpreter = iter
            .next()
            .expect("split ensured at least one token exists");
        let remaining: Vec<String> = iter.collect();
        if Self::is_env_invocation(&raw_interpreter) {
            self.resolve_env_interpreter(script, resolver, origin, raw_interpreter, remaining)
        } else {
            self.resolve_direct_interpreter(script, resolver, origin, raw_interpreter, remaining)
        }
    }

    fn resolve_direct_interpreter(
        &self,
        script: &Path,
        resolver: &dyn PathResolver,
        origin: &Origin,
        interpreter: String,
        args: Vec<String>,
    ) -> Result<ShebangSpec, ClosureError> {
        let candidate = PathBuf::from(&interpreter);
        if !candidate.is_absolute() {
            return Err(ClosureError::ShebangParse {
                path: script.to_path_buf(),
                reason: "interpreter path must be absolute".into(),
            });
        }
        if let Some(resolved) = self.resolve_in_origin(resolver, origin, &candidate) {
            Ok(ShebangSpec {
                interpreter_host: resolved,
                args,
            })
        } else {
            Err(ClosureError::InterpreterNotFound {
                script: script.to_path_buf(),
                interpreter,
            })
        }
    }

    fn resolve_env_interpreter(
        &self,
        script: &Path,
        resolver: &dyn PathResolver,
        origin: &Origin,
        interpreter: String,
        mut args: Vec<String>,
    ) -> Result<ShebangSpec, ClosureError> {
        if args.is_empty() {
            return Err(ClosureError::ShebangParse {
                path: script.to_path_buf(),
                reason: format!("{interpreter} requires a command to execute"),
            });
        }
        let command = args.remove(0);
        if command.starts_with('-') {
            return Err(ClosureError::ShebangParse {
                path: script.to_path_buf(),
                reason: format!("env-style shebang flags are not supported (saw `{command}`)"),
            });
        }
        if let Some(resolved) = self.resolve_command(resolver, origin, &command) {
            Ok(ShebangSpec {
                interpreter_host: resolved,
                args,
            })
        } else {
            Err(ClosureError::InterpreterNotFound {
                script: script.to_path_buf(),
                interpreter: command,
            })
        }
    }

    fn resolve_command(
        &self,
        resolver: &dyn PathResolver,
        origin: &Origin,
        command: &str,
    ) -> Option<PathBuf> {
        let candidate = PathBuf::from(command);
        if candidate.components().count() > 1 || candidate.is_absolute() {
            return self.resolve_in_origin(resolver, origin, &candidate);
        }
        let search_paths = self
            .origin_paths
            .get(origin)
            .cloned()
            .unwrap_or_else(|| self.shebang_path_entries());
        for dir in search_paths {
            let joined = dir.join(command);
            if let Some(resolved) = self.resolve_in_origin(resolver, origin, &joined) {
                return Some(resolved);
            }
        }
        None
    }

    fn resolve_in_origin(
        &self,
        resolver: &dyn PathResolver,
        origin: &Origin,
        candidate: &Path,
    ) -> Option<PathBuf> {
        let logical = LogicalPath::new(origin.clone(), candidate.to_path_buf());
        let host = resolver.to_host(&logical);
        match fs::metadata(&host) {
            Ok(meta) if meta.is_file() => {}
            _ => return None,
        }
        canonicalize(&host, resolver.trace_root()).ok()
    }

    fn shebang_path_entries(&self) -> Vec<PathBuf> {
        env::var("PATH")
            .ok()
            .map(|value| Self::split_paths(&value))
            .filter(|paths| !paths.is_empty())
            .unwrap_or_else(|| {
                DEFAULT_SHEBANG_PATHS
                    .iter()
                    .map(|entry| PathBuf::from(entry))
                    .collect()
            })
    }

    fn is_env_invocation(interpreter: &str) -> bool {
        let path = Path::new(interpreter);
        match path.file_name().and_then(|n| n.to_str()) {
            Some("env") => true,
            _ => false,
        }
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

    fn compute_search_paths(
        &self,
        resolver: &dyn PathResolver,
        origin: &Origin,
        binary: &Path,
        metadata: &ElfMetadata,
    ) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        let binary_dir = binary.parent().unwrap_or_else(|| Path::new("/"));
        let preferred = if metadata.runpaths.is_empty() {
            &metadata.rpaths
        } else {
            &metadata.runpaths
        };

        for segment in preferred {
            if let Some(path) = Self::expand_origin(segment, binary_dir) {
                paths.push(path);
            }
        }

        paths.extend(Self::resolve_additional_paths(
            resolver,
            origin,
            &self.ld_library_paths,
        ));
        paths.push(binary_dir.to_path_buf());
        paths.extend(Self::resolve_additional_paths(
            resolver,
            origin,
            &self.default_paths,
        ));
        paths
    }


    fn resolve_additional_paths(
        resolver: &dyn PathResolver,
        origin: &Origin,
        entries: &[PathBuf],
    ) -> Vec<PathBuf> {
        entries
            .iter()
            .map(|path| {
                let logical = LogicalPath::new(origin.clone(), path.clone());
                resolver.to_host(&logical)
            })
            .collect()
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

    fn is_bash_interpreter(interpreter: &Path) -> bool {
        interpreter
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n == "bash" || n == "sh")
            .unwrap_or(false)
    }

    fn collect_bash_dependencies(
        &self,
        resolver: &dyn PathResolver,
        origin: &Origin,
        script: &Path,
        files: &mut BTreeMap<PathBuf, PathBuf>,
        aliases: &mut HashMap<PathBuf, BTreeSet<PathBuf>>,
        cache: &mut HashMap<PathBuf, ElfMetadata>,
    ) {
        let commands = Self::scan_bash_commands(script);
        let mut visited: HashSet<PathBuf> = HashSet::new();
        for cmd in commands {
            if let Some(resolved) = self.resolve_command(resolver, origin, &cmd) {
                if !visited.insert(resolved.clone()) {
                    continue;
                }
                let _ = self.build_binary_plan(
                    resolver,
                    origin,
                    &resolved,
                    &cmd,
                    files,
                    aliases,
                    cache,
                );
            }
        }
    }

    fn scan_bash_commands(script: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let Ok(data) = fs::read_to_string(script) else {
            return out;
        };
        let mut seen: HashSet<String> = HashSet::new();
        let re_line = Regex::new(r"(?m)^[ \t]*([A-Za-z0-9_./-]+)").unwrap();
        let re_dollar = Regex::new(r"\$\(\s*([A-Za-z0-9_./-]+)").unwrap();
        let re_backtick = Regex::new(r"`\s*([A-Za-z0-9_./-]+)").unwrap();
        let re_pipe = Regex::new(r"\|\s*([A-Za-z0-9_./-]+)").unwrap();
        let re_abs = Regex::new(r"(/[-A-Za-z0-9_./]+)").unwrap();
        let keywords = [
            "if", "then", "fi", "for", "do", "done", "elif", "else", "while", "case", "esac",
            "function", "in",
        ];
        for caps in re_line.captures_iter(&data) {
            if let Some(mat) = caps.get(1) {
                let token = mat.as_str();
                if keywords.iter().any(|k| *k == token) {
                    continue;
                }
                if seen.insert(token.to_string()) {
                    out.push(token.to_string());
                }
            }
        }
        for caps in re_dollar.captures_iter(&data) {
            if let Some(mat) = caps.get(1) {
                let token = mat.as_str();
                if seen.insert(token.to_string()) {
                    out.push(token.to_string());
                }
            }
        }
        for caps in re_backtick.captures_iter(&data) {
            if let Some(mat) = caps.get(1) {
                let token = mat.as_str();
                if seen.insert(token.to_string()) {
                    out.push(token.to_string());
                }
            }
        }
        for caps in re_pipe.captures_iter(&data) {
            if let Some(mat) = caps.get(1) {
                let token = mat.as_str();
                if seen.insert(token.to_string()) {
                    out.push(token.to_string());
                }
            }
        }
        for caps in re_abs.captures_iter(&data) {
            if let Some(mat) = caps.get(1) {
                let token = mat.as_str();
                if seen.insert(token.to_string()) {
                    out.push(token.to_string());
                }
            }
        }
        out
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
        match self.runner.trace_dependencies(linker, subject, search_paths) {
            Ok(resolved) => Ok(resolved),
            Err(linker_err) => {
                if matches!(linker_err, linker::LinkerError::UnsupportedStub { .. }) {
                    warn!(
                        "linker trace skipped for {} (unsupported stub: {})",
                        subject.display(),
                        linker_err
                    );
                    return Ok(Vec::new());
                }
                Err(ClosureError::LinkerTrace {
                    path: subject.to_path_buf(),
                    source: linker_err,
                })
            }
        }
    }

    pub fn split_paths(value: &str) -> Vec<PathBuf> {
        value
            .split(':')
            .filter(|segment| !segment.trim().is_empty())
            .map(PathBuf::from)
            .collect()
    }

    fn find_host_glibc_linker() -> Option<PathBuf> {
        for candidate in HOST_GLIBC_LINKERS {
            let path = PathBuf::from(candidate);
            match fs::metadata(&path) {
                Ok(meta) if meta.is_file() => {
                    if let Ok(is_stub) = is_gcompat_stub_binary(&path) {
                        if is_stub {
                            continue;
                        }
                    }
                    return Some(path);
                }
                _ => continue,
            }
        }
        None
    }

    fn trace_command_for(
        &self,
        entry: &BundleEntry,
        plan: &EntryBundlePlan,
        resolver: &dyn PathResolver,
    ) -> Option<trace::TraceCommand> {
        match plan {
            EntryBundlePlan::Binary(_) => {
                let mut command = trace::TraceCommand::new(entry.logical.clone());
                if let Some(args) = &entry.trace_args {
                    command = command.with_args(args.clone());
                }
                Some(command)
            }
            EntryBundlePlan::Script(script) => {
                let interpreter_logical = resolver.host_to_logical(&script.interpreter_source)?;
                let script_runtime = resolver.to_trace_path(&entry.logical);
                let mut args = script.interpreter_args.clone();
                args.push(script_runtime.display().to_string());
                if let Some(extra) = &entry.trace_args {
                    args.extend(extra.clone());
                }
                Some(trace::TraceCommand::new(interpreter_logical).with_args(args))
            }
        }
    }
}

fn canonicalize(path: &Path, root: Option<&Path>) -> Result<PathBuf, ClosureError> {
    let target = if let Some(root) = root {
        if path.starts_with(root) {
            path.to_path_buf()
        } else if path.is_absolute() {
            let rel = path.strip_prefix("/").unwrap_or(path);
            root.join(rel)
        } else {
            root.join(path)
        }
    } else {
        path.to_path_buf()
    };
    debug!(
        "canonicalize request path {} root {:?} => candidate {}",
        path.display(),
        root.map(|r| r.display().to_string()),
        target.display()
    );
    match fs::canonicalize(&target) {
        Ok(resolved) => {
            if let Some(root) = root {
                if !resolved.starts_with(root) && path.is_absolute() {
                    debug!(
                        "canonicalize {} resolved outside root {}; returning candidate {}",
                        path.display(),
                        root.display(),
                        target.display()
                    );
                    return Ok(target);
                }
            }
            debug!(
                "canonicalize {} resolved to {}",
                path.display(),
                resolved.display()
            );
            Ok(resolved)
        }
        Err(source) => Err(ClosureError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn ensure_file(
    resolver: &dyn PathResolver,
    mapping: &mut BTreeMap<PathBuf, PathBuf>,
    aliases: &mut HashMap<PathBuf, BTreeSet<PathBuf>>,
    source: &Path,
    runtime_alias: Option<&Path>,
) -> PathBuf {
    let canonical_logical = resolver
        .host_to_logical(source)
        .map(|logical| logical.path().to_path_buf());
    if let Some(dest) = mapping.get(source) {
        if let Some(alias) = runtime_alias {
            if should_record_alias(canonical_logical.as_deref(), alias) {
                record_alias(aliases, source, alias);
            }
        }
        return dest.clone();
    }
    let mut dest = PathBuf::from("payload");
    if let Some(logical_path) = canonical_logical.as_deref() {
        if logical_path.is_absolute() {
            for component in logical_path.components().skip(1) {
                dest.push(component.as_os_str());
            }
        } else {
            dest.push(logical_path);
        }
    } else if source.is_absolute() {
        for component in source.components().skip(1) {
            dest.push(component.as_os_str());
        }
    } else {
        dest.push(source);
    }
    mapping.insert(source.to_path_buf(), dest.clone());
    if let Some(alias) = runtime_alias {
        if should_record_alias(canonical_logical.as_deref(), alias) {
            record_alias(aliases, source, alias);
        }
    }
    dest
}

fn should_record_alias(canonical: Option<&Path>, alias: &Path) -> bool {
    match canonical {
        Some(path) => path != alias,
        None => true,
    }
}

fn record_alias(
    aliases: &mut HashMap<PathBuf, BTreeSet<PathBuf>>,
    source: &Path,
    runtime_path: &Path,
) {
    let entry = aliases.entry(source.to_path_buf()).or_default();
    entry.insert(runtime_path.to_path_buf());
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

fn trace_path_allowed(path: &Path) -> bool {
    if path.as_os_str().is_empty() {
        return false;
    }
    for prefix in TRACE_SKIP_PREFIXES {
        if path.starts_with(Path::new(prefix)) {
            return false;
        }
    }
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        if TRACE_SKIP_FILENAMES.iter().any(|skip| skip == &name) {
            return false;
        }
    }
    match fs::metadata(path) {
        Ok(meta) => meta.is_file(),
        Err(_) => false,
    }
}

fn classify_high_risk_asset(path: &Path) -> Option<&'static str> {
    for component in path.components() {
        if let Some(name) = component.as_os_str().to_str() {
            if name.eq_ignore_ascii_case(GLIBC_HWCAPS_SEGMENT) {
                return Some("glibc-hwcaps");
            }
        }
    }
    if let Some(file) = path.file_name().and_then(|n| n.to_str()) {
        let lower = file.to_ascii_lowercase();
        for prefix in GPU_LIB_PREFIXES {
            if lower.starts_with(prefix) {
                return Some("gpu-driver");
            }
        }
    }
    None
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
    #[error("unsupported entry {path}: expected ELF or shebang script")]
    UnsupportedEntry { path: PathBuf },
    #[error("failed to parse shebang for {path}: {reason}")]
    ShebangParse { path: PathBuf, reason: String },
    #[error("interpreter `{interpreter}` not found for script {script}")]
    InterpreterNotFound {
        script: PathBuf,
        interpreter: String,
    },
    #[error("binary {path} lacks PT_INTERP linker")]
    MissingInterpreter { path: PathBuf },
    #[error("linker trace failed for {path}: {source}")]
    LinkerTrace { path: PathBuf, source: LinkerError },
    #[error("no resolver registered for origin {origin:?}")]
    MissingResolver { origin: Origin },
}

#[cfg(test)]
mod tests {
    use super::*;
    use sidebundle_core::{BundleSpec, TargetTriple};
    use std::collections::HashSet as TestHashSet;

    #[test]
    fn closure_collects_host_binary() {
        #[cfg(target_os = "linux")]
        {
            let spec = BundleSpec::new("demo", TargetTriple::linux_x86_64())
                .with_entry(BundleSpec::host_entry("/bin/ls", "ls"));
            let mut builder = ClosureBuilder::new();
            let closure = builder.build(&spec).unwrap();
            assert!(
                !closure.files.is_empty(),
                "expected /bin/ls closure to contain files"
            );
            assert!(
                closure
                    .entry_plans
                    .iter()
                    .any(|plan| plan.display_name() == "ls"),
                "entry plan should include launcher info"
            );
        }
    }

    #[test]
    fn trace_filter_skips_virtual_fs() {
        assert!(!trace_path_allowed(Path::new("/proc/self/maps")));
        assert!(!trace_path_allowed(Path::new("/sys/devices/system/cpu")));
    }

    #[test]
    fn trace_filter_skips_locale_archive() {
        assert!(!trace_path_allowed(Path::new(
            "/usr/lib/locale/locale-archive"
        )));
    }

    #[test]
    fn trace_filter_keeps_regular_file() {
        let path = env::current_dir().unwrap().join("Cargo.toml");
        assert!(trace_path_allowed(&path));
    }

    #[test]
    fn classify_blocks_glibc_hwcaps() {
        let path = Path::new("/usr/lib/glibc-hwcaps/x86-64-v2/libc.so.6");
        assert_eq!(
            classify_high_risk_asset(path),
            Some("glibc-hwcaps"),
            "expected glibc-hwcaps path to be filtered"
        );
    }

    #[test]
    fn classify_blocks_gpu_drivers() {
        let path = Path::new("/usr/lib/x86_64-linux-gnu/libcuda.so.1");
        assert_eq!(
            classify_high_risk_asset(path),
            Some("gpu-driver"),
            "expected libcuda to be filtered"
        );
    }

    #[test]
    fn scan_bash_commands_finds_literal_invocations() {
        let script = Path::new("tests/fixtures/scip-java");
        let commands = ClosureBuilder::scan_bash_commands(script);
        let set: TestHashSet<_> = commands.into_iter().collect();
        for expected in ["coursier", "java", "jq", "tr", "/app/scip-java"] {
            assert!(
                set.contains(expected),
                "expected bash scanner to find {expected}"
            );
        }
    }
}
