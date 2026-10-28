pub mod image;
mod linker;
pub mod trace;
pub mod validator;

use crate::image::ImageRoot;
use log::warn;
use std::cell::RefCell;
use std::collections::{btree_map::Entry, BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use linker::{is_gcompat_stub_binary, LibraryResolution, LinkerError, LinkerRunner};
use log::debug;
use regex::Regex;
use sha2::{Digest, Sha256};
use sidebundle_core::{
    parse_elf_metadata, BinaryEntryPlan, BundleEntry, BundleSpec, DependencyClosure, ElfMetadata,
    ElfParseError, EntryBundlePlan, LogicalPath, Origin, ResolvedFile, ResolvedSymlink,
    ScriptEntryPlan, TraceAccess, TracedFile,
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
// Real GL/GLES/GLX/EGL sonames are matched in `is_gpu_library_name`. A bare
// `libgl` prefix also matches `libglib-2.0` and `libglibmm`.

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
        if let Ok(rel) = host_path.strip_prefix(&self.root) {
            if let Some(logical) = rebuild_logical(rel.to_path_buf(), &self.origin) {
                return Some(logical);
            }
        }
        // fallback: try pathdiff to strip root even if components differ (e.g., canonicalized tmp prefixes)
        if let Some(rel) = pathdiff::diff_paths(host_path, &self.root) {
            if let Some(logical) = rebuild_logical(rel, &self.origin) {
                return Some(logical);
            }
        }
        // special-case temp export prefixes (/tmp/.tmpXXXX/...), strip until after the temp dir.
        if let Some(stripped) = strip_tmp_prefix(host_path) {
            if let Some(logical) = rebuild_logical(stripped, &self.origin) {
                return Some(logical);
            }
        }
        None
    }
}

fn rebuild_logical(rel: PathBuf, origin: &Origin) -> Option<LogicalPath> {
    let mut comps = rel.components().peekable();
    if let Some(first) = comps.peek() {
        if first.as_os_str() == "rootfs" {
            comps.next();
        }
    }
    let mut rebuilt = PathBuf::from("/");
    for comp in comps {
        rebuilt.push(comp.as_os_str());
    }
    if rebuilt == Path::new("/") {
        return None;
    }
    Some(LogicalPath::new(origin.clone(), rebuilt))
}

fn strip_tmp_prefix(path: &Path) -> Option<PathBuf> {
    let mut comps = path.components();
    let first = comps.next()?; // RootDir
    let second = comps.next()?;
    if first.as_os_str() != "/" || second.as_os_str() != "tmp" {
        return None;
    }
    let third = comps.next()?;
    let third_str = third.as_os_str().to_string_lossy();
    if !third_str.starts_with(".tmp") {
        return None;
    }
    // Skip optional "rootfs" layer after temp dir.
    let mut comps_iter = comps.peekable();
    if let Some(peek) = comps_iter.peek() {
        if peek.as_os_str() == "rootfs" {
            comps_iter.next();
        }
    }
    let mut rebuilt = PathBuf::from("/");
    for c in comps_iter {
        rebuilt.push(c.as_os_str());
    }
    Some(rebuilt)
}

#[derive(Clone)]
pub struct ResolverSet {
    resolvers: HashMap<Origin, Arc<dyn PathResolver>>,
}

struct PlanState<'a> {
    files: &'a mut BTreeMap<PathBuf, PathBuf>,
    aliases: &'a mut HashMap<PathBuf, BTreeSet<PathBuf>>,
    cache: &'a mut HashMap<PathBuf, ElfMetadata>,
}

impl<'a> PlanState<'a> {
    fn new(
        files: &'a mut BTreeMap<PathBuf, PathBuf>,
        aliases: &'a mut HashMap<PathBuf, BTreeSet<PathBuf>>,
        cache: &'a mut HashMap<PathBuf, ElfMetadata>,
    ) -> Self {
        Self {
            files,
            aliases,
            cache,
        }
    }
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
        map.insert(Origin::Host, Arc::new(HostPathResolver));
        Self { resolvers: map }
    }
}

/// Builds dependency closures for host executables.
pub struct ClosureBuilder {
    ld_library_paths: Vec<PathBuf>,
    default_paths: Vec<PathBuf>,
    origin_paths: HashMap<Origin, Vec<PathBuf>>,
    scanned_scripts: RefCell<HashSet<PathBuf>>,
    /// DT_NEEDED paths that must ship even when they look like GPU libraries.
    /// The dynamic linker cannot start ffmpeg without libdrm/libGLX; dlopen'd
    /// driver files that are not in this set stay filtered.
    load_time_deps: RefCell<HashSet<PathBuf>>,
    allow_gpu_libs: bool,
    runner: LinkerRunner,
    tracer: Option<trace::TraceCollector>,
    resolvers: ResolverSet,
    external_traces: HashMap<Origin, Vec<trace::TraceSpecRecord>>,
}

impl ClosureBuilder {
    pub fn new() -> Self {
        Self {
            ld_library_paths: env::var("LD_LIBRARY_PATH")
                .ok()
                .map(|value| Self::split_paths(&value))
                .unwrap_or_default(),
            default_paths: DEFAULT_LIBRARY_DIRS.iter().map(PathBuf::from).collect(),
            origin_paths: HashMap::new(),
            scanned_scripts: RefCell::new(HashSet::new()),
            load_time_deps: RefCell::new(HashSet::new()),
            allow_gpu_libs: false,
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

    pub fn with_allow_gpu_libs(mut self, allow: bool) -> Self {
        self.allow_gpu_libs = allow;
        self
    }

    /// Override LD_LIBRARY_PATH search paths for linker tracing.
    pub fn with_ld_library_paths(mut self, paths: Vec<PathBuf>) -> Self {
        self.ld_library_paths = paths;
        self
    }

    pub fn with_origin_path(mut self, origin: Origin, path: Vec<PathBuf>) -> Self {
        self.origin_paths.insert(origin, path);
        self
    }

    pub fn with_external_trace_paths(mut self, origin: Origin, paths: Vec<PathBuf>) -> Self {
        let records = paths
            .into_iter()
            .map(|path| trace::TraceSpecRecord {
                path,
                access: TraceAccess::OPEN,
            })
            .collect::<Vec<_>>();
        self.external_traces
            .entry(origin)
            .or_default()
            .extend(records);
        self
    }

    pub fn with_external_trace_records(
        mut self,
        origin: Origin,
        records: Vec<trace::TraceSpecRecord>,
    ) -> Self {
        self.external_traces
            .entry(origin)
            .or_default()
            .extend(records);
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
        let mut symlinks: Vec<ResolvedSymlink> = Vec::new();

        for entry in spec.entries() {
            let resolver = self.resolver_for(entry.logical.origin())?;
            let mut plan = self.build_entry(
                entry,
                resolver.as_ref(),
                &mut file_map,
                &mut runtime_aliases,
                &mut elf_cache,
            )?;
            match &mut plan {
                EntryBundlePlan::Binary(p) => p.run_mode = Some(spec.run_mode()),
                EntryBundlePlan::Script(p) => p.run_mode = Some(spec.run_mode()),
            }
            if let Some(tracer) = &self.tracer {
                if let Some(command) = self.trace_command_for(entry, &plan, resolver.as_ref()) {
                    match tracer.run(resolver.as_ref(), &command) {
                        Ok(artifacts) => {
                            let origin = plan.origin().clone();
                            let origin_map = traced_map.entry(origin).or_default();
                            for record in artifacts {
                                if let Some(artifact) =
                                    self.make_trace_artifact(resolver.as_ref(), &record)
                                {
                                    Self::insert_traced_file(origin_map, artifact);
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
            for record in runtime_paths {
                if let Some(host_path) = resolver.runtime_to_host(&record.path) {
                    let canonical_host = match canonicalize(&host_path, resolver.trace_root()) {
                        Ok(path) => path,
                        Err(err) => {
                            warn!(
                                "skipping traced path {}: failed to canonicalize ({err})",
                                record.path.display()
                            );
                            continue;
                        }
                    };
                    let artifact = trace::TraceArtifact {
                        runtime_path: record.path.clone(),
                        host_path: Some(canonical_host.clone()),
                        logical_path: Some(LogicalPath::new(origin.clone(), record.path.clone())),
                        access: record.access,
                    };
                    if let Some(traced) = self.make_trace_artifact(resolver.as_ref(), &artifact) {
                        Self::insert_traced_file(origin_map, traced);
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
            self.record_traced_symlinks(resolver.as_ref(), &mut symlinks, &traced_files);
            traced_files_acc.extend(traced_files);
        }

        let mut files = Vec::new();
        let mut seen_destinations: HashSet<PathBuf> = HashSet::new();
        for (source, destination) in file_map.into_iter() {
            if let Some(reason) = self.filter_reason(&source) {
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
            symlinks,
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

    fn make_trace_artifact(
        &self,
        resolver: &dyn PathResolver,
        original: &trace::TraceArtifact,
    ) -> Option<TracedFile> {
        let resolved = original.host_path.as_ref()?;
        if !trace_path_allowed(resolved) {
            return None;
        }
        if let Some(reason) = self.filter_reason(resolved) {
            debug!(
                "skipping traced artifact {} (filtered: {reason})",
                resolved.display()
            );
            return None;
        }
        let host_path = match canonicalize(resolved, resolver.trace_root()) {
            Ok(path) => path,
            Err(err) => {
                warn!(
                    "skipping traced artifact {}: failed to canonicalize ({err})",
                    resolved.display()
                );
                return None;
            }
        };
        let is_elf = parse_elf_metadata(&host_path).is_ok();
        Some(TracedFile {
            original: original.runtime_path.clone(),
            resolved: host_path,
            is_elf,
            access: original.access,
        })
    }

    fn insert_traced_file(map: &mut BTreeMap<PathBuf, TracedFile>, artifact: TracedFile) {
        match map.entry(artifact.original.clone()) {
            Entry::Vacant(entry) => {
                entry.insert(artifact);
            }
            Entry::Occupied(mut entry) => {
                let existing = entry.get_mut();
                existing.access.insert(artifact.access);
            }
        }
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
        let mut state = PlanState::new(files, aliases, cache);
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
            let _ =
                self.build_entry_plan(resolver, origin, &artifact.resolved, display, &mut state)?;
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
            if let Some(reason) = self.filter_reason(source) {
                debug!("skipping traced resource {} ({reason})", source.display());
                continue;
            }
            let _ = ensure_file(resolver, files, aliases, source, Some(&artifact.original));
        }
    }

    fn record_traced_symlinks(
        &self,
        resolver: &dyn PathResolver,
        symlinks: &mut Vec<ResolvedSymlink>,
        traced: &[TracedFile],
    ) {
        let mut seen: HashSet<(PathBuf, PathBuf)> = HashSet::new();
        for artifact in traced {
            if !self.should_preserve_symlink(resolver, artifact) {
                continue;
            }
            let Some(target_logical) = resolver.host_to_logical(&artifact.resolved) else {
                continue;
            };
            let destination = artifact.original.clone();
            let bundle_target = target_logical.path().to_path_buf();
            if destination == bundle_target {
                continue;
            }
            if seen.insert((destination.clone(), bundle_target.clone())) {
                symlinks.push(ResolvedSymlink::new(destination, bundle_target));
            }
        }
    }

    fn should_preserve_symlink(&self, resolver: &dyn PathResolver, traced: &TracedFile) -> bool {
        if traced.access.contains(TraceAccess::LINK) {
            return true;
        }
        if !(traced.access.contains(TraceAccess::EXEC) || traced.access.contains(TraceAccess::OPEN))
        {
            return false;
        }
        let Some(host_path) = resolver.runtime_to_host(&traced.original) else {
            return false;
        };
        match fs::symlink_metadata(&host_path) {
            Ok(meta) => meta.file_type().is_symlink(),
            Err(_) => false,
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
        let entry_source =
            prefer_real_elf(&entry_source, resolver.trace_root(), &entry.display_name)?;
        let mut state = PlanState::new(files, aliases, cache);
        self.build_entry_plan(
            resolver,
            entry.logical.origin(),
            &entry_source,
            &entry.display_name,
            &mut state,
        )
    }

    fn build_entry_plan(
        &self,
        resolver: &dyn PathResolver,
        origin: &Origin,
        entry_source: &Path,
        display_name: &str,
        state: &mut PlanState<'_>,
    ) -> Result<EntryBundlePlan, ClosureError> {
        match parse_elf_metadata(entry_source) {
            Ok(metadata) => {
                state
                    .cache
                    .entry(entry_source.to_path_buf())
                    .or_insert_with(|| metadata.clone());
                let plan =
                    self.build_binary_plan(resolver, origin, entry_source, display_name, state)?;
                Ok(EntryBundlePlan::Binary(plan))
            }
            Err(ElfParseError::NotElf { .. }) => {
                let shebang = self.parse_shebang(entry_source, resolver, origin)?;
                let interpreter_plan = self.build_binary_plan(
                    resolver,
                    origin,
                    &shebang.interpreter_host,
                    display_name,
                    state,
                )?;
                let runtime_alias = resolver
                    .host_to_logical(entry_source)
                    .map(|logical| logical.path().to_path_buf());
                let script_destination = ensure_file(
                    resolver,
                    state.files,
                    state.aliases,
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
                    run_mode: None,
                };
                if Self::is_bash_interpreter(&plan.interpreter_source) {
                    self.collect_bash_dependencies(resolver, origin, entry_source, state);
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
        state: &mut PlanState<'_>,
    ) -> Result<BinaryEntryPlan, ClosureError> {
        let entry_metadata = self.load_metadata(entry_source, state.cache)?;
        let entry_dest = ensure_file(resolver, state.files, state.aliases, entry_source, None);

        let (mut interpreter_source, mut interpreter_dest, is_static) =
            match entry_metadata.interpreter.clone() {
                Some(path) => {
                    let canonical = canonicalize(&path, resolver.trace_root())?;
                    let dest = ensure_file(
                        resolver,
                        state.files,
                        state.aliases,
                        &canonical,
                        Some(&path),
                    );
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

            let metadata = self.load_metadata(&current, state.cache)?;
            if is_static {
                continue;
            }
            let interpreter = interpreter_source.as_ref().expect("static skipped");
            let search_paths = self.compute_search_paths(resolver, origin, &current, metadata);
            let resolved =
                self.trace_with_linker(interpreter, &current, &search_paths, metadata)?;

            for resolution in resolved {
                if Self::should_skip(&resolution.name) {
                    continue;
                }
                let canonical = canonicalize(&resolution.target, resolver.trace_root())?;
                if self.skip_loaded_dependency(&canonical) {
                    debug!(
                        "skipping dependency {} for {} ({})",
                        canonical.display(),
                        current.display(),
                        classify_high_risk_asset(&canonical).unwrap_or("filtered")
                    );
                    continue;
                }
                let alias_runtime = resolver
                    .host_to_logical(&resolution.target)
                    .map(|logical| logical.path().to_path_buf());
                let dest = ensure_file(
                    resolver,
                    state.files,
                    state.aliases,
                    &canonical,
                    alias_runtime.as_deref(),
                );
                if let Some(dir) = dest.parent() {
                    lib_dirs.insert(dir.to_path_buf());
                }
                record_soname_alias(resolver, state.aliases, &canonical, &resolution.name, &dest);
                queue.push_back(canonical);
            }
        }

        if visited.iter().any(|path| is_magick_path(path)) || is_magick_path(entry_source) {
            let root = resolver.trace_root().unwrap_or_else(|| Path::new("/"));
            let modules = discover_imagemagick_modules(root);
            self.ingest_layout_modules(
                resolver,
                origin,
                &modules,
                &mut visited,
                &mut lib_dirs,
                state,
                interpreter_source.as_deref(),
                is_static,
            );
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
                        let desired_dest = state.files.remove(&stub_source).unwrap_or_else(|| {
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
                        state.files.insert(host_ld.clone(), desired_dest.clone());
                        interpreter_source = Some(host_ld.clone());
                        interpreter_dest = Some(desired_dest);
                        debug!(
                            "gcompat replacement: interpreter_source={}, interpreter_dest={}",
                            host_ld.display(),
                            interpreter_dest
                                .as_ref()
                                .map(|p| p.display().to_string())
                                .unwrap_or_else(|| "<none>".into())
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
            run_mode: None,
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
        debug!("resolving command `{command}` in origin {origin:?} with PATH {search_paths:?}");
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
            .unwrap_or_else(|| DEFAULT_SHEBANG_PATHS.iter().map(PathBuf::from).collect())
    }

    fn is_env_invocation(interpreter: &str) -> bool {
        let path = Path::new(interpreter);
        matches!(path.file_name().and_then(|n| n.to_str()), Some("env"))
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

    fn filter_reason(&self, path: &Path) -> Option<&'static str> {
        match classify_high_risk_asset(path) {
            Some("gpu-driver")
                if self.allow_gpu_libs || self.load_time_deps.borrow().contains(path) =>
            {
                None
            }
            other => other,
        }
    }

    /// glibc-hwcaps stays out. A GPU-looking DT_NEEDED (libdrm, libGLX, …) is
    /// recorded and kept: the loader needs it before main. Traced dlopen hits
    /// are still dropped unless `--allow-gpu-libs` is set.
    fn skip_loaded_dependency(&self, path: &Path) -> bool {
        match classify_high_risk_asset(path) {
            Some("gpu-driver") => {
                self.load_time_deps.borrow_mut().insert(path.to_path_buf());
                false
            }
            Some(_) => true,
            None => false,
        }
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
        state: &mut PlanState<'_>,
    ) {
        let commands = Self::scan_bash_commands(script);
        {
            let mut seen = self.scanned_scripts.borrow_mut();
            if !seen.insert(script.to_path_buf()) {
                debug!(
                    "bash static scan: skipping already scanned script {}",
                    script.display()
                );
                return;
            }
        }
        let mut visited: HashSet<PathBuf> = HashSet::new();
        for cmd in commands {
            match self.resolve_command(resolver, origin, &cmd) {
                Some(resolved) => {
                    if !visited.insert(resolved.clone()) {
                        continue;
                    }
                    debug!(
                        "bash static scan: resolved command `{cmd}` -> {}",
                        resolved.display()
                    );
                    let _ = self.build_entry_plan(resolver, origin, &resolved, &cmd, state);
                }
                None => {
                    debug!("bash static scan: command `{cmd}` not found via PATH for origin {origin:?}");
                }
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
                if keywords.contains(&token) {
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
        let listing = match self
            .runner
            .trace_dependencies_partial(linker, subject, search_paths)
        {
            Ok(listing) => listing,
            Err(linker_err) => {
                if matches!(linker_err, linker::LinkerError::UnsupportedStub { .. }) {
                    warn!(
                        "linker trace skipped for {} (unsupported stub: {})",
                        subject.display(),
                        linker_err
                    );
                    return Ok(Vec::new());
                }
                let mut resolved = Vec::new();
                let still = augment_resolutions(&mut resolved, subject, metadata, search_paths);
                if still.is_empty() {
                    warn!(
                        "linker trace failed for {} ({linker_err}); resolved DT_NEEDED via directory search",
                        subject.display()
                    );
                    return Ok(resolved);
                }
                return Err(ClosureError::LinkerTrace {
                    path: subject.to_path_buf(),
                    source: linker_err,
                });
            }
        };
        let mut resolved = listing.resolved;
        for name in &listing.missing {
            if resolution_satisfies(&resolved, name) || should_skip_needed(name) {
                continue;
            }
            if let Some(path) = find_needed_library(name, subject, search_paths) {
                resolved.push(LibraryResolution {
                    name: name.clone(),
                    target: path,
                });
            }
        }
        let still = augment_resolutions(&mut resolved, subject, metadata, search_paths);
        if !still.is_empty() {
            return Err(ClosureError::LinkerTrace {
                path: subject.to_path_buf(),
                source: linker::LinkerError::LibraryNotFound {
                    name: still[0].clone(),
                    raw: format!(
                        "missing after linker and private-directory search: {}",
                        still.join(", ")
                    ),
                },
            });
        }
        Ok(resolved)
    }

    /// Pull ImageMagick coder/filter modules into the closure and walk their DT_NEEDED.
    /// Failures are warnings: a single optional coder must not abort the pack.
    #[allow(clippy::too_many_arguments)]
    fn ingest_layout_modules(
        &self,
        resolver: &dyn PathResolver,
        origin: &Origin,
        modules: &[PathBuf],
        visited: &mut HashSet<PathBuf>,
        lib_dirs: &mut BTreeSet<PathBuf>,
        state: &mut PlanState<'_>,
        interpreter_source: Option<&Path>,
        is_static: bool,
    ) {
        if modules.is_empty() || is_static {
            return;
        }
        let Some(interpreter) = interpreter_source else {
            return;
        };
        let mut queue: VecDeque<PathBuf> = VecDeque::new();
        for module in modules {
            let canonical = match canonicalize(module, resolver.trace_root()) {
                Ok(path) => path,
                Err(err) => {
                    debug!("skipping ImageMagick module {}: {err}", module.display());
                    continue;
                }
            };
            let dest = ensure_file(resolver, state.files, state.aliases, &canonical, None);
            if let Some(dir) = dest.parent() {
                lib_dirs.insert(dir.to_path_buf());
            }
            queue.push_back(canonical);
        }
        while let Some(current) = queue.pop_front() {
            if !visited.insert(current.clone()) {
                continue;
            }
            let metadata = match self.load_metadata(&current, state.cache) {
                Ok(metadata) => metadata.clone(),
                Err(ClosureError::ElfParse {
                    source: ElfParseError::NotElf { .. },
                    ..
                }) => continue,
                Err(err) => {
                    warn!("skipping ImageMagick module {}: {err}", current.display());
                    continue;
                }
            };
            let search_paths = self.compute_search_paths(resolver, origin, &current, &metadata);
            let resolved = match self.trace_with_linker(
                interpreter,
                &current,
                &search_paths,
                &metadata,
            ) {
                Ok(resolved) => resolved,
                Err(err) => {
                    warn!(
                        "ImageMagick module {} is missing libraries ({err}); keeping the module file",
                        current.display()
                    );
                    continue;
                }
            };
            for resolution in resolved {
                if Self::should_skip(&resolution.name) {
                    continue;
                }
                let canonical = match canonicalize(&resolution.target, resolver.trace_root()) {
                    Ok(path) => path,
                    Err(err) => {
                        debug!(
                            "skipping ImageMagick dependency {}: {err}",
                            resolution.target.display()
                        );
                        continue;
                    }
                };
                if self.skip_loaded_dependency(&canonical) {
                    debug!(
                        "skipping ImageMagick dependency {} ({})",
                        canonical.display(),
                        classify_high_risk_asset(&canonical).unwrap_or("filtered")
                    );
                    continue;
                }
                let dest = ensure_file(resolver, state.files, state.aliases, &canonical, None);
                if let Some(dir) = dest.parent() {
                    lib_dirs.insert(dir.to_path_buf());
                }
                record_soname_alias(resolver, state.aliases, &canonical, &resolution.name, &dest);
                queue.push_back(canonical);
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

impl Default for ClosureBuilder {
    fn default() -> Self {
        Self::new()
    }
}

fn canonicalize(path: &Path, root: Option<&Path>) -> Result<PathBuf, ClosureError> {
    let Some(root) = root else {
        return fs::canonicalize(path).map_err(|source| ClosureError::Io {
            path: path.to_path_buf(),
            source,
        });
    };
    // Resolve the root directory itself, then walk symlinks without leaving it.
    // `fs::canonicalize` follows absolute links (Debian alternatives) onto the host.
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    debug!(
        "canonicalize request path {} root {}",
        path.display(),
        root.display()
    );
    canonicalize_in_root(&root, path)
}

fn canonicalize_in_root(root: &Path, path: &Path) -> Result<PathBuf, ClosureError> {
    let initial = if path.starts_with(root) {
        path.to_path_buf()
    } else if path.is_absolute() {
        root.join(path.strip_prefix("/").unwrap_or(path))
    } else {
        root.join(path)
    };
    let mut stack: Vec<std::ffi::OsString> = Vec::new();
    if let Ok(rel) = initial.strip_prefix(root) {
        for component in rel.components() {
            if let std::path::Component::Normal(part) = component {
                stack.push(part.to_os_string());
            }
        }
    }
    let mut index = 0;
    let mut hops = 0;
    let mut last_link: Option<(PathBuf, PathBuf)> = None;
    while index < stack.len() {
        hops += 1;
        if hops > 64 {
            return Err(ClosureError::EntryResolution {
                path: path.to_path_buf(),
                message: format!("symlink cycle while resolving {}", path.display()),
            });
        }
        let mut current = root.to_path_buf();
        for part in stack.iter().take(index + 1) {
            current.push(part);
        }
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = fs::read_link(&current).map_err(|source| ClosureError::Io {
                    path: current.clone(),
                    source,
                })?;
                last_link = Some((current, target.clone()));
                let mut rebuilt = Vec::new();
                if !target.is_absolute() {
                    rebuilt.extend(stack.iter().take(index).cloned());
                }
                for component in target.components() {
                    match component {
                        std::path::Component::Normal(part) => rebuilt.push(part.to_os_string()),
                        std::path::Component::ParentDir => {
                            rebuilt.pop();
                        }
                        std::path::Component::CurDir | std::path::Component::RootDir => {}
                        std::path::Component::Prefix(_) => {}
                    }
                }
                rebuilt.extend(stack.iter().skip(index + 1).cloned());
                stack = rebuilt;
                index = 0;
            }
            Ok(_) => index += 1,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Err(dangling_entry_error(path, &current, last_link.as_ref()));
            }
            Err(source) => {
                return Err(ClosureError::Io {
                    path: current,
                    source,
                });
            }
        }
    }
    let mut resolved = root.to_path_buf();
    for part in &stack {
        resolved.push(part);
    }
    if !resolved.exists() {
        return Err(dangling_entry_error(path, &resolved, last_link.as_ref()));
    }
    debug!(
        "canonicalize {} resolved inside root to {}",
        path.display(),
        resolved.display()
    );
    Ok(resolved)
}

fn dangling_entry_error(
    requested: &Path,
    missing: &Path,
    last_link: Option<&(PathBuf, PathBuf)>,
) -> ClosureError {
    let message = if let Some((link, target)) = last_link {
        let hint = if target.to_string_lossy().contains("alternatives") {
            "This looks like a Debian alternatives symlink. Pass the real ELF (ImageMagick convert is often /usr/bin/convert-im6.q16)."
        } else {
            "Pass the real ELF path instead of a wrapper or dangling symlink."
        };
        format!(
            "entry {} follows symlink {} -> {}, which does not exist inside the bundle root (missing {}). {hint}",
            requested.display(),
            link.display(),
            target.display(),
            missing.display()
        )
    } else {
        format!(
            "path {} does not exist inside the bundle root (missing {})",
            requested.display(),
            missing.display()
        )
    };
    ClosureError::EntryResolution {
        path: requested.to_path_buf(),
        message,
    }
}

fn prefer_real_elf(
    path: &Path,
    root: Option<&Path>,
    entry_name: &str,
) -> Result<PathBuf, ClosureError> {
    if parse_elf_metadata(path).is_ok() {
        return Ok(path.to_path_buf());
    }
    if !looks_like_pg_wrapper(path) {
        return Ok(path.to_path_buf());
    }
    // `/usr/bin/psql` is a relative symlink to `pg_wrapper`. Search with the
    // name the user asked for, not the script's own filename.
    let name = entry_tool_name(path, entry_name);
    let root = root.unwrap_or_else(|| Path::new("/"));
    let matches = find_postgresql_elfs(root, &name);
    match matches.len() {
        1 => {
            warn!(
                "entry {} is a PostgreSQL pg_wrapper script; using real ELF {}",
                path.display(),
                matches[0].display()
            );
            Ok(matches[0].clone())
        }
        0 => Err(ClosureError::EntryResolution {
            path: path.to_path_buf(),
            message: format!(
                "entry {} is a Perl pg_wrapper script, not an ELF. Pass the real client binary, typically /usr/lib/postgresql/<version>/bin/{name}",
                path.display()
            ),
        }),
        _ => {
            let listed = matches
                .iter()
                .map(|candidate| candidate.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            Err(ClosureError::EntryResolution {
                path: path.to_path_buf(),
                message: format!(
                    "entry {} is a Perl pg_wrapper script and multiple ELFs match: {listed}. Pass one of those paths explicitly.",
                    path.display()
                ),
            })
        }
    }
}

fn entry_tool_name(path: &Path, entry_name: &str) -> String {
    let trimmed = entry_name.trim();
    if !trimmed.is_empty() && !trimmed.contains('/') {
        return trimmed.to_string();
    }
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("psql")
        .to_string()
}

fn looks_like_pg_wrapper(path: &Path) -> bool {
    let mut buf = [0u8; 4096];
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let Ok(read) = file.read(&mut buf) else {
        return false;
    };
    if buf.starts_with(b"\x7fELF") {
        return false;
    }
    let text = String::from_utf8_lossy(&buf[..read]);
    text.contains("PgCommon") || text.contains("pg_wrapper")
}

fn is_elf_file(path: &Path) -> bool {
    let mut buf = [0u8; 4];
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    matches!(file.read(&mut buf), Ok(4) if &buf == b"\x7fELF")
}

fn find_postgresql_elfs(root: &Path, name: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(versions) = fs::read_dir(root.join("usr/lib/postgresql")) else {
        return found;
    };
    for version in versions.flatten() {
        let candidate = version.path().join("bin").join(name);
        if is_elf_file(&candidate) {
            found.push(candidate);
        }
    }
    found.sort();
    found
}

fn should_skip_needed(name: &str) -> bool {
    let base = Path::new(name)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(name);
    base.starts_with("linux-vdso") || base.starts_with("ld-linux") || base.starts_with("ld-musl")
}

fn resolution_satisfies(resolved: &[LibraryResolution], name: &str) -> bool {
    resolved.iter().any(|item| {
        item.name == name
            || item
                .target
                .file_name()
                .and_then(|file| file.to_str())
                .is_some_and(|file| file == name)
    })
}

fn augment_resolutions(
    resolved: &mut Vec<LibraryResolution>,
    subject: &Path,
    metadata: &ElfMetadata,
    search_paths: &[PathBuf],
) -> Vec<String> {
    let mut missing = Vec::new();
    for name in &metadata.needed {
        if should_skip_needed(name) || resolution_satisfies(resolved, name) {
            continue;
        }
        if let Some(path) = find_needed_library(name, subject, search_paths) {
            resolved.push(LibraryResolution {
                name: name.clone(),
                target: path,
            });
        } else {
            missing.push(name.clone());
        }
    }
    missing
}

/// Locate `libfoo.so` beside the subject, on the linker search path, or in an immediate
/// private subdirectory such as `pulseaudio/libpulsecommon-*.so`.
fn find_needed_library(name: &str, subject: &Path, search_paths: &[PathBuf]) -> Option<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(parent) = subject.parent() {
        dirs.push(parent.to_path_buf());
    }
    dirs.extend(search_paths.iter().cloned());
    for dir in &dirs {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let parent = subject.parent()?;
    let entries = fs::read_dir(parent).ok()?;
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let candidate = entry.path().join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn is_magick_path(path: &Path) -> bool {
    let text = path.to_string_lossy();
    if text.contains("ImageMagick") {
        return true;
    }
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.starts_with("libMagick") || name == "magick" || name.starts_with("convert-im")
        })
}

fn discover_imagemagick_modules(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for base in [
        "usr/lib",
        "usr/lib64",
        "usr/local/lib",
        "usr/lib/x86_64-linux-gnu",
        "lib",
        "lib64",
    ] {
        walk_magick_modules(&root.join(base), 0, &mut found);
    }
    found.sort();
    found.dedup();
    found
}

fn walk_magick_modules(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    if depth > 7 || !dir.is_dir() {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            walk_magick_modules(&path, depth + 1, found);
            continue;
        }
        if !kind.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !(name.ends_with(".so") || name.contains(".so.") || name.ends_with(".la")) {
            continue;
        }
        let text = path.to_string_lossy();
        if text.contains("ImageMagick") && (text.contains("/coders/") || text.contains("/filters/"))
        {
            found.push(path);
        }
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

fn record_soname_alias(
    resolver: &dyn PathResolver,
    aliases: &mut HashMap<PathBuf, BTreeSet<PathBuf>>,
    canonical: &Path,
    needed_name: &str,
    dest: &Path,
) {
    if needed_name.is_empty() {
        return;
    }
    if let Some(logical) = resolver.host_to_logical(canonical) {
        let mut runtime_alias = logical.path().to_path_buf();
        if runtime_alias.file_name().and_then(|name| name.to_str()) != Some(needed_name) {
            runtime_alias.set_file_name(needed_name);
            record_alias(aliases, canonical, &runtime_alias);
        }
        return;
    }
    if let Some(parent) = dest.parent() {
        let alias_path = parent.join(needed_name);
        if alias_path != dest {
            record_alias(aliases, canonical, &alias_path);
        }
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
        write!(&mut hex, "{byte:02x}").expect("write to string");
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
        if is_gpu_library_name(file) {
            return Some("gpu-driver");
        }
    }
    None
}

fn is_gpu_library_name(file: &str) -> bool {
    let lower = file.to_ascii_lowercase();
    const PREFIXES: &[&str] = &[
        "libgl.so",
        "libglx",
        "libgldispatch",
        "libgles",
        "libegl",
        "libopengl",
        "libopencl",
        "libvulkan",
        "libcuda",
        "libnvidia",
        "libdrm",
        "libnvoptix",
        "libopenvr",
        "libxnvctrl",
    ];
    PREFIXES.iter().any(|prefix| lower.starts_with(prefix))
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
    #[error("{message}")]
    EntryResolution { path: PathBuf, message: String },
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
    fn classify_does_not_treat_glib_as_a_gpu_library() {
        assert_eq!(
            classify_high_risk_asset(Path::new("/usr/lib/x86_64-linux-gnu/libglib-2.0.so.0")),
            None
        );
        assert_eq!(
            classify_high_risk_asset(Path::new("/usr/lib/libglibmm-2.4.so.1")),
            None
        );
        assert_eq!(
            classify_high_risk_asset(Path::new("/usr/lib/libGL.so.1")),
            Some("gpu-driver")
        );
        assert_eq!(
            classify_high_risk_asset(Path::new("/usr/lib/libGLX.so.0")),
            Some("gpu-driver")
        );
        assert_eq!(
            classify_high_risk_asset(Path::new("/usr/lib/libGLdispatch.so.0")),
            Some("gpu-driver")
        );
        assert_eq!(
            classify_high_risk_asset(Path::new("/usr/lib/libGLESv2.so.2")),
            Some("gpu-driver")
        );
        assert_eq!(
            classify_high_risk_asset(Path::new("/lib/x86_64-linux-gnu/libdrm.so.2")),
            Some("gpu-driver")
        );
    }

    #[test]
    fn dt_needed_gpu_library_is_kept_when_traced_copies_are_not() {
        let builder = ClosureBuilder::new();
        let drm = Path::new("/usr/lib/x86_64-linux-gnu/libdrm.so.2");
        let cuda = Path::new("/usr/lib/libcuda.so.1");
        let glib = Path::new("/usr/lib/x86_64-linux-gnu/libglib-2.0.so.0");
        assert_eq!(builder.filter_reason(drm), Some("gpu-driver"));
        assert_eq!(builder.filter_reason(glib), None);
        assert!(!builder.skip_loaded_dependency(drm));
        assert_eq!(builder.filter_reason(drm), None);
        assert_eq!(builder.filter_reason(cuda), Some("gpu-driver"));
        assert!(builder
            .skip_loaded_dependency(Path::new("/usr/lib/glibc-hwcaps/x86-64-v3/libdrm.so.2")));
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

    #[test]
    fn traced_access_is_merged_per_runtime_path() {
        let mut map = BTreeMap::new();
        let base = TracedFile {
            original: PathBuf::from("/etc/hosts"),
            resolved: PathBuf::from("/etc/hosts"),
            is_elf: false,
            access: TraceAccess::OPEN,
        };
        ClosureBuilder::insert_traced_file(&mut map, base.clone());
        let mut second = base;
        second.access = TraceAccess::STAT;
        ClosureBuilder::insert_traced_file(&mut map, second);

        let merged = map.get(Path::new("/etc/hosts")).expect("merged entry");
        assert!(merged.access.contains(TraceAccess::OPEN));
        assert!(merged.access.contains(TraceAccess::STAT));
    }

    #[test]
    fn in_root_canonicalize_follows_alternatives_and_reports_dangling() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("usr/bin/convert-im6.q16");
        fs::create_dir_all(real.parent().unwrap()).unwrap();
        fs::write(&real, b"\x7fELF").unwrap();
        let alternatives = root.path().join("etc/alternatives");
        fs::create_dir_all(&alternatives).unwrap();
        std::os::unix::fs::symlink("/usr/bin/convert-im6.q16", alternatives.join("convert"))
            .unwrap();
        let bin = root.path().join("usr/bin");
        std::os::unix::fs::symlink("/etc/alternatives/convert", bin.join("convert")).unwrap();

        let resolved = canonicalize(Path::new("/usr/bin/convert"), Some(root.path())).unwrap();
        assert_eq!(resolved, real);

        let dangling_root = tempfile::tempdir().unwrap();
        let dangling_bin = dangling_root.path().join("usr/bin");
        fs::create_dir_all(&dangling_bin).unwrap();
        std::os::unix::fs::symlink("/etc/alternatives/convert", dangling_bin.join("convert"))
            .unwrap();
        let err =
            canonicalize(Path::new("/usr/bin/convert"), Some(dangling_root.path())).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("alternatives"),
            "expected alternatives hint, got {message}"
        );
        assert!(
            message.contains("convert-im6.q16"),
            "expected real-path hint, got {message}"
        );
    }

    #[test]
    fn pg_wrapper_prefers_single_real_elf_and_lists_ambiguous_ones() {
        let root = tempfile::tempdir().unwrap();
        let wrapper = root.path().join("usr/bin/psql");
        fs::create_dir_all(wrapper.parent().unwrap()).unwrap();
        fs::write(&wrapper, b"#!/usr/bin/perl\nuse PgCommon;\n").unwrap();
        let real = root.path().join("usr/lib/postgresql/17/bin/psql");
        fs::create_dir_all(real.parent().unwrap()).unwrap();
        fs::write(&real, b"\x7fELF").unwrap();

        let chosen = prefer_real_elf(&wrapper, Some(root.path()), "psql").unwrap();
        assert_eq!(chosen, real);

        fs::create_dir_all(root.path().join("usr/lib/postgresql/16/bin")).unwrap();
        fs::write(
            root.path().join("usr/lib/postgresql/16/bin/psql"),
            b"\x7fELF",
        )
        .unwrap();
        let err = prefer_real_elf(&wrapper, Some(root.path()), "psql").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("multiple ELFs"), "{message}");
        assert!(
            message.contains("/usr/lib/postgresql/16/bin/psql"),
            "{message}"
        );
        assert!(
            message.contains("/usr/lib/postgresql/17/bin/psql"),
            "{message}"
        );
    }

    #[test]
    fn pg_wrapper_symlink_searches_the_entry_basename() {
        let root = tempfile::tempdir().unwrap();
        let script_dir = root.path().join("usr/share/postgresql-common");
        fs::create_dir_all(&script_dir).unwrap();
        let script = script_dir.join("pg_wrapper");
        fs::write(&script, b"#!/usr/bin/perl\nuse PgCommon;\n# pg_wrapper\n").unwrap();
        let bin = root.path().join("usr/bin");
        fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink("../share/postgresql-common/pg_wrapper", bin.join("psql"))
            .unwrap();
        let real = root.path().join("usr/lib/postgresql/17/bin/psql");
        fs::create_dir_all(real.parent().unwrap()).unwrap();
        fs::write(&real, b"\x7fELF").unwrap();

        let resolved = canonicalize(Path::new("/usr/bin/psql"), Some(root.path())).unwrap();
        assert!(resolved.ends_with("pg_wrapper"), "{}", resolved.display());
        let chosen = prefer_real_elf(&resolved, Some(root.path()), "psql").unwrap();
        assert_eq!(chosen, real);

        let err = prefer_real_elf(&resolved, Some(root.path()), "pg_wrapper").unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("/usr/lib/postgresql/<version>/bin/pg_wrapper"),
            "{message}"
        );
    }

    #[test]
    fn private_subdir_resolves_pulse_common_and_direct_needed() {
        let root = tempfile::tempdir().unwrap();
        let libdir = root.path().join("usr/lib/x86_64-linux-gnu");
        let private = libdir.join("pulseaudio");
        fs::create_dir_all(&private).unwrap();
        let subject = libdir.join("libpulse.so.0");
        fs::write(&subject, b"pulse").unwrap();
        let common = private.join("libpulsecommon-16.1.so");
        fs::write(&common, b"common").unwrap();

        let found = find_needed_library("libpulsecommon-16.1.so", &subject, &[]).unwrap();
        assert_eq!(found, common);

        let glib = libdir.join("libglib-2.0.so.0");
        fs::write(&glib, b"glib").unwrap();
        let metadata = ElfMetadata {
            interpreter: None,
            needed: vec![
                "libglib-2.0.so.0".into(),
                "libpulsecommon-16.1.so".into(),
                "linux-vdso.so.1".into(),
            ],
            rpaths: Vec::new(),
            runpaths: Vec::new(),
            soname: None,
        };
        let mut resolved = vec![LibraryResolution {
            name: "libglib-2.0.so.0".into(),
            target: glib.clone(),
        }];
        let missing = augment_resolutions(&mut resolved, &subject, &metadata, &[]);
        assert!(missing.is_empty(), "{missing:?}");
        assert!(resolved.iter().any(|item| item.target == common));
        assert!(resolved.iter().any(|item| item.target == glib));
    }

    #[test]
    fn imagemagick_layout_collects_coder_modules_only() {
        let root = tempfile::tempdir().unwrap();
        let coder = root
            .path()
            .join("usr/lib/x86_64-linux-gnu/ImageMagick-6.9.11/modules-Q16/coders");
        fs::create_dir_all(&coder).unwrap();
        fs::write(coder.join("png.so"), b"png").unwrap();
        fs::write(coder.join("png.la"), b"dlname='png.so'\n").unwrap();
        let filter = root
            .path()
            .join("usr/lib/x86_64-linux-gnu/ImageMagick-6.9.11/modules-Q16/filters");
        fs::create_dir_all(&filter).unwrap();
        fs::write(filter.join("analyze.so"), b"filter").unwrap();
        let unrelated = root.path().join("usr/lib/x86_64-linux-gnu/libfoo.so.1");
        fs::write(&unrelated, b"foo").unwrap();

        let found = discover_imagemagick_modules(root.path());
        assert!(found.iter().any(|path| path.ends_with("png.so")));
        assert!(found.iter().any(|path| path.ends_with("png.la")));
        assert!(found.iter().any(|path| path.ends_with("analyze.so")));
        assert!(!found.iter().any(|path| path.ends_with("libfoo.so.1")));
    }
}
