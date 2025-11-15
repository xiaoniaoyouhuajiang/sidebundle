use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use env_logger::Env;
use log::{debug, info, warn, LevelFilter};
use serde::Deserialize;
use shell_words;
use sidebundle_closure::{
    image::{DockerProvider, ImageRoot, ImageRootProvider, PodmanProvider},
    trace::{TraceBackendKind, TraceCollector},
    validator::{BundleValidator, EntryValidationStatus, LinkerFailure, ValidationReport},
    ChrootPathResolver, ClosureBuilder, ResolverSet,
};
use sidebundle_core::{
    BundleEntry, BundleSpec, DependencyClosure, LogicalPath, MergeReport, Origin, TargetTriple,
};
use sidebundle_packager::Packager;

fn main() {
    let cli = Cli::parse();
    if let Err(err) = init_logger(cli.log_level) {
        eprintln!("sidebundle: failed to init logger: {err}");
        std::process::exit(1);
    }

    if let Err(err) = run(cli) {
        eprintln!("sidebundle: {err:?}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Create(args) => execute_create(args),
        Commands::Agent(agent) => execute_agent(agent),
    }
}

fn execute_agent(cmd: AgentCommands) -> Result<()> {
    match cmd {
        AgentCommands::Trace(args) => execute_agent_trace(args),
    }
}

fn execute_create(args: CreateArgs) -> Result<()> {
    let CreateArgs {
        from_host,
        from_image,
        image_backend,
        name,
        target,
        out_dir,
        trace_root,
        trace_backend,
        image_trace_backend,
        strict_validate,
    } = args;

    if from_host.is_empty() && from_image.is_empty() {
        bail!("at least one --from-host or --from-image entry is required");
    }

    let target = TargetTriple::parse(&target)
        .with_context(|| format!("unsupported target triple: {}", target))?;

    info!(
        "building bundle `{}` for target {} ({} host entries, {} image entries)",
        name,
        target,
        from_host.len(),
        from_image.len()
    );

    let mut spec = BundleSpec::new(name, target);
    for (idx, entry) in from_host.iter().enumerate() {
        std::fs::metadata(&entry.path)
            .with_context(|| format!("failed to read host executable: {}", entry.path.display()))?;
        let display = entry
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_owned())
            .unwrap_or_else(|| format!("entry-{idx}"));
        let mut bundle_entry = BundleSpec::host_entry(entry.path.clone(), display);
        if let Some(args) = &entry.trace_args {
            bundle_entry = bundle_entry.with_trace_args(args.clone());
        }
        spec.push_entry(bundle_entry);
    }

    let host_backend =
        resolve_trace_backend(trace_backend).context("failed to configure host trace backend")?;
    let image_backend_choice = image_trace_backend.unwrap_or(trace_backend);
    let image_backend_kind = resolve_trace_backend(image_backend_choice)
        .context("failed to configure image trace backend")?;

    let mut host_resolvers = ResolverSet::new();
    if let Some(root) = &trace_root {
        let resolver = Arc::new(ChrootPathResolver::from_root(root.clone(), Origin::Host));
        host_resolvers.insert(Origin::Host, resolver);
    }
    let mut builder = ClosureBuilder::new().with_resolver_set(host_resolvers.clone());
    if let Some(backend) = host_backend.clone() {
        let tracer = TraceCollector::new().with_backend(backend);
        builder = builder.with_tracer(tracer);
    }

    let mut closure = builder
        .build(&spec)
        .context("failed to build dependency closure")?;
    log_closure_stats("host inputs", &closure);

    let default_backend = BackendPreference::from(image_backend);
    let grouped = group_image_entries(&from_image, default_backend)?;
    let mut _resolver_guards = Vec::new();
    for ((preference, reference), entries) in grouped {
        info!(
            "constructing closure for image `{}` via {:?} backend ({} entr{} )",
            reference,
            preference,
            entries.len(),
            if entries.len() == 1 { "y" } else { "ies" }
        );
        let image_result = build_image_closure(
            preference,
            &reference,
            &entries,
            target,
            image_backend_kind.clone(),
        )
        .with_context(|| {
            format!("failed to build closure for image `{reference}` using backend {preference:?}")
        })?;
        log_closure_stats(&format!("image `{reference}`"), &image_result.closure);
        let report = closure.merge(image_result.closure);
        log_merge_report(&reference, &report);
        _resolver_guards.push(image_result.resolvers);
    }

    if closure.entry_plans.is_empty() {
        bail!("no executable entries were collected from host or image inputs");
    }

    let packager = if let Some(dir) = out_dir {
        Packager::new().with_output_root(dir)
    } else {
        Packager::new()
    };
    let output = packager
        .emit(&spec, &closure)
        .context("packaging stage failed")?;
    info!(
        "bundle `{}` (target {}) written to {}",
        spec.name(),
        spec.target(),
        output.display()
    );
    let validator = BundleValidator::new();
    let report = validator.validate_with_report(&output, &closure.entry_plans);
    log_validation_report(&report);
    if !report.all_passed() {
        let mut details = String::new();
        for entry in report.failures() {
            let _ = writeln!(
                &mut details,
                " - {} => {}",
                entry.display_name,
                describe_status(&entry.status)
            );
        }
        if strict_validate {
            bail!("bundle validation failed:\n{}", details.trim_end());
        } else {
            warn!(
                "validation found {} entr{} with issues; rerun with --strict-validate to fail build",
                report.failure_count(),
                if report.failure_count() == 1 { "y" } else { "ies" }
            );
            warn!("{}", details.trim_end());
        }
    }
    info!("linker validation succeeded");
    Ok(())
}

fn execute_agent_trace(args: AgentTraceArgs) -> Result<()> {
    fs::create_dir_all(&args.output)
        .with_context(|| format!("failed to create output dir {}", args.output.display()))?;
    let spec_data = fs::read(&args.spec)
        .with_context(|| format!("failed to read spec file {}", args.spec.display()))?;
    let agent_spec: AgentSpec =
        serde_json::from_slice(&spec_data).with_context(|| "failed to parse agent spec json")?;

    let target = TargetTriple::parse(&agent_spec.target)
        .with_context(|| format!("unsupported target triple: {}", agent_spec.target))?;
    let mut spec = BundleSpec::new(agent_spec.name.clone(), target);
    let agent_origin = Origin::Image(agent_spec.name.clone());
    for (idx, entry) in agent_spec.entries.iter().enumerate() {
        let mut host_path = entry.path.clone();
        if !host_path.is_absolute() {
            host_path = args.rootfs.join(&host_path);
        }
        std::fs::metadata(&host_path).with_context(|| {
            format!(
                "agent entry {} not found at {}",
                entry.path.display(),
                host_path.display()
            )
        })?;
        let display = entry
            .display
            .clone()
            .or_else(|| {
                entry
                    .path
                    .file_name()
                    .and_then(|n| n.to_str().map(|s| s.to_string()))
            })
            .unwrap_or_else(|| format!("agent-entry-{idx}"));
        let logical = LogicalPath::new(agent_origin.clone(), entry.path.clone());
        let mut bundle_entry = BundleEntry::new(logical, display);
        if let Some(args) = &entry.trace {
            bundle_entry = bundle_entry.with_trace_args(args.clone());
        }
        spec.push_entry(bundle_entry);
    }

    let mut resolver_set = ResolverSet::new();
    let agent_resolver = Arc::new(ChrootPathResolver::from_root(
        args.rootfs.clone(),
        agent_origin.clone(),
    ));
    resolver_set.insert(agent_origin.clone(), agent_resolver);
    let mut builder = ClosureBuilder::new().with_resolver_set(resolver_set.clone());
    let agent_backend = resolve_trace_backend(args.trace_backend)
        .context("failed to configure agent trace backend")?;
    if let Some(kind) = agent_backend {
        let env_pairs = parse_image_env(&agent_spec.env);
        let mut tracer = TraceCollector::new().with_backend(kind);
        if !env_pairs.is_empty() {
            tracer = tracer.with_env(env_pairs);
        }
        builder = builder.with_tracer(tracer);
    }

    let closure = builder
        .build(&spec)
        .context("agent: failed to build dependency closure")?;
    let packager = Packager::new().with_output_root(&args.output);
    packager
        .emit(&spec, &closure)
        .context("agent: packaging stage failed")?;
    Ok(())
}

fn init_logger(level: LogLevel) -> Result<()> {
    let env = Env::default().default_filter_or(level.as_str());
    env_logger::Builder::from_env(env)
        .format_timestamp_secs()
        .filter_level(LevelFilter::from(level))
        .try_init()
        .map_err(|err| err.into())
}

#[derive(Parser)]
#[command(name = "sidebundle", version, about = "Minimal sidecar bundler CLI")]
struct Cli {
    #[arg(long = "log-level", value_enum, default_value_t = LogLevel::Info, global = true)]
    log_level: LogLevel,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create bundle artifacts
    Create(CreateArgs),
    /// Internal helper to run trace/build stages inside a container
    #[command(subcommand, hide = true)]
    Agent(AgentCommands),
}

#[derive(Args)]
struct CreateArgs {
    /// Executable specs on the host (PATH[::trace=<cmd>])
    #[arg(
        long = "from-host",
        value_name = "SPEC",
        value_parser = parse_host_entry,
        num_args = 0..
    )]
    from_host: Vec<HostEntryArg>,

    /// Image reference and path pairs (format: [backend://]IMAGE::/path[::trace=<cmd>])
    #[arg(
        long = "from-image",
        value_name = "SPEC",
        value_parser = parse_image_entry,
        num_args = 0..,
    )]
    from_image: Vec<ImageEntryArg>,

    /// Default image backend when not specified inline
    #[arg(long = "image-backend", value_enum, default_value_t = ImageBackendArg::Auto)]
    image_backend: ImageBackendArg,

    /// Bundle name
    #[arg(long = "name", default_value = "bundle")]
    name: String,

    /// Target triple
    #[arg(long = "target", default_value = "linux-x86_64")]
    target: String,

    /// Output root (defaults to target/bundles)
    #[arg(long = "out-dir", value_name = "DIR")]
    out_dir: Option<PathBuf>,

    /// Optional rootfs for linker/tracer chroot
    #[arg(long = "trace-root", value_name = "DIR")]
    trace_root: Option<PathBuf>,

    /// Runtime trace backend for host inputs
    #[arg(long = "trace-backend", value_enum, default_value_t = TraceBackendArg::Auto)]
    trace_backend: TraceBackendArg,

    /// Runtime trace backend for image inputs
    #[arg(long = "image-trace-backend", value_enum)]
    image_trace_backend: Option<TraceBackendArg>,

    /// Fail the build when linker validation finds missing dependencies
    #[arg(long = "strict-validate")]
    strict_validate: bool,
}

#[derive(Args)]
struct AgentTraceArgs {
    /// Absolute bundle root inside the container
    #[arg(long = "rootfs", value_name = "DIR")]
    rootfs: PathBuf,

    /// JSON file describing the build spec
    #[arg(long = "spec", value_name = "FILE")]
    spec: PathBuf,

    /// Output directory to write closure artifacts
    #[arg(long = "output", value_name = "DIR")]
    output: PathBuf,

    /// Trace backend to use inside the agent
    #[arg(long = "trace-backend", value_enum, default_value_t = TraceBackendArg::Auto)]
    trace_backend: TraceBackendArg,
}

#[derive(Debug, Deserialize)]
struct AgentSpec {
    name: String,
    target: String,
    entries: Vec<AgentSpecEntry>,
    #[serde(default)]
    env: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct AgentSpecEntry {
    path: PathBuf,
    #[serde(default)]
    display: Option<String>,
    #[serde(default)]
    trace: Option<Vec<String>>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum TraceBackendArg {
    Off,
    Auto,
    Ptrace,
    Fanotify,
    Combined,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum ImageBackendArg {
    Auto,
    Docker,
    Podman,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        }
    }
}

impl From<LogLevel> for LevelFilter {
    fn from(value: LogLevel) -> Self {
        match value {
            LogLevel::Error => LevelFilter::Error,
            LogLevel::Warn => LevelFilter::Warn,
            LogLevel::Info => LevelFilter::Info,
            LogLevel::Debug => LevelFilter::Debug,
            LogLevel::Trace => LevelFilter::Trace,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
enum BackendPreference {
    Auto,
    Docker,
    Podman,
}

impl From<ImageBackendArg> for BackendPreference {
    fn from(value: ImageBackendArg) -> Self {
        match value {
            ImageBackendArg::Auto => BackendPreference::Auto,
            ImageBackendArg::Docker => BackendPreference::Docker,
            ImageBackendArg::Podman => BackendPreference::Podman,
        }
    }
}

#[derive(Debug, Clone)]
struct HostEntryArg {
    path: PathBuf,
    trace_args: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
struct ImageEntryArg {
    backend: Option<BackendPreference>,
    reference: String,
    path: PathBuf,
    trace_args: Option<Vec<String>>,
}

fn parse_host_entry(value: &str) -> Result<HostEntryArg, String> {
    let (path_part, trace_part) = split_trace_clause(value);
    let trimmed = path_part.trim();
    if trimmed.is_empty() {
        return Err("host entry path cannot be empty".into());
    }
    let path = PathBuf::from(trimmed);
    let trace_args = match trace_part {
        Some(spec) => Some(parse_trace_args(spec)?),
        None => None,
    };
    Ok(HostEntryArg { path, trace_args })
}

fn parse_image_entry(value: &str) -> Result<ImageEntryArg, String> {
    let (entry_part, trace_part) = split_trace_clause(value);
    let (image_part, path_part) = entry_part
        .split_once("::")
        .ok_or_else(|| "expected format IMAGE::/path/in/image".to_string())?;
    if path_part.trim().is_empty() {
        return Err("image entry path cannot be empty".into());
    }
    let path = PathBuf::from(path_part);
    if !path.is_absolute() {
        return Err("image entry path must be absolute".into());
    }
    let (backend, reference) = if let Some(rest) = image_part.strip_prefix("docker://") {
        (Some(BackendPreference::Docker), rest.to_string())
    } else if let Some(rest) = image_part.strip_prefix("podman://") {
        (Some(BackendPreference::Podman), rest.to_string())
    } else {
        (None, image_part.to_string())
    };
    if reference.trim().is_empty() {
        return Err("image reference cannot be empty".into());
    }
    let trace_args = match trace_part {
        Some(spec) => Some(parse_trace_args(spec)?),
        None => None,
    };
    Ok(ImageEntryArg {
        backend,
        reference: reference.trim().to_string(),
        path,
        trace_args,
    })
}

fn split_trace_clause(value: &str) -> (&str, Option<&str>) {
    if let Some(idx) = value.find("::trace=") {
        let (entry, rest) = value.split_at(idx);
        let spec = &rest["::trace=".len()..];
        (entry, Some(spec))
    } else {
        (value, None)
    }
}

fn parse_trace_args(spec: &str) -> Result<Vec<String>, String> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Err("trace command cannot be empty".into());
    }
    shell_words::split(trimmed).map_err(|err| format!("failed to parse trace command: {err}"))
}

fn group_image_entries(
    inputs: &[ImageEntryArg],
    default_backend: BackendPreference,
) -> Result<BTreeMap<(BackendPreference, String), Vec<ImageEntryArg>>> {
    let mut map: BTreeMap<(BackendPreference, String), Vec<ImageEntryArg>> = BTreeMap::new();
    for input in inputs {
        let preference = input.backend.unwrap_or(default_backend);
        map.entry((preference, input.reference.clone()))
            .or_default()
            .push(input.clone());
    }
    Ok(map)
}

struct ImageClosureResult {
    closure: DependencyClosure,
    resolvers: ResolverSet,
}

fn build_image_closure(
    preference: BackendPreference,
    reference: &str,
    entries: &[ImageEntryArg],
    target: TargetTriple,
    trace_backend: Option<TraceBackendKind>,
) -> Result<ImageClosureResult> {
    if entries.is_empty() {
        bail!("no entry paths provided for image `{reference}`");
    }
    let attempts: Vec<BackendPreference> = match preference {
        BackendPreference::Auto => vec![BackendPreference::Docker, BackendPreference::Podman],
        other => vec![other],
    };
    let mut errors = Vec::new();
    for backend in attempts {
        match build_with_backend(backend, reference, entries, target, trace_backend.clone()) {
            Ok(result) => return Ok(result),
            Err(err) => {
                errors.push(format!("{backend:?}: {err:?}"));
            }
        }
    }
    bail!(
        "all image providers failed for `{reference}`:\n{}",
        errors.join("\n")
    );
}

fn build_with_backend(
    backend: BackendPreference,
    reference: &str,
    entries: &[ImageEntryArg],
    target: TargetTriple,
    trace_backend: Option<TraceBackendKind>,
) -> Result<ImageClosureResult> {
    match backend {
        BackendPreference::Docker => {
            let provider = DockerProvider::new();
            let root = provider
                .prepare_root(reference)
                .with_context(|| "docker provider invocation failed")?;
            build_closure_from_root(
                "docker",
                reference,
                target,
                &root,
                entries,
                trace_backend.clone(),
            )
        }
        BackendPreference::Podman | BackendPreference::Auto => {
            let provider = PodmanProvider::new();
            let root = provider
                .prepare_root(reference)
                .with_context(|| "podman provider invocation failed")?;
            build_closure_from_root("podman", reference, target, &root, entries, trace_backend)
        }
    }
}

fn build_closure_from_root(
    backend_name: &'static str,
    reference: &str,
    target: TargetTriple,
    root: &ImageRoot,
    entries: &[ImageEntryArg],
    trace_backend: Option<TraceBackendKind>,
) -> Result<ImageClosureResult> {
    let rootfs = root.rootfs().to_path_buf();
    let image_env = parse_image_env(&root.config().env);
    let mut spec = BundleSpec::new(format!("{backend_name}:{reference}"), target);
    let origin = Origin::Image(reference.to_string());
    for (idx, entry_spec) in entries.iter().enumerate() {
        let physical = physical_image_path(&rootfs, &entry_spec.path);
        std::fs::metadata(&physical).with_context(|| {
            format!(
                "image `{reference}` missing executable {} (resolved path {})",
                entry_spec.path.display(),
                physical.display()
            )
        })?;
        let display = entry_spec
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("image-entry-{idx}"));
        let logical = LogicalPath::new(origin.clone(), entry_spec.path.clone());
        let mut entry = BundleEntry::new(logical, display);
        if let Some(args) = &entry_spec.trace_args {
            entry = entry.with_trace_args(args.clone());
        }
        spec.push_entry(entry);
    }

    let mut resolvers = ResolverSet::new();
    let chroot_resolver = Arc::new(ChrootPathResolver::from_image(root.clone(), origin.clone()));
    resolvers.insert(origin.clone(), chroot_resolver);

    let mut builder = ClosureBuilder::new().with_resolver_set(resolvers.clone());
    if let Some(backend) = trace_backend {
        let mut tracer = TraceCollector::new().with_backend(backend);
        if !image_env.is_empty() {
            tracer = tracer.with_env(image_env.clone());
        }
        builder = builder.with_tracer(tracer);
    }

    let mut closure = builder
        .build(&spec)
        .with_context(|| format!("failed to build closure inside image `{reference}`"))?;
    normalize_image_closure(&mut closure, &rootfs);
    Ok(ImageClosureResult { closure, resolvers })
}

fn physical_image_path(rootfs: &Path, virtual_path: &Path) -> PathBuf {
    let relative = virtual_path
        .strip_prefix("/")
        .unwrap_or(virtual_path)
        .to_path_buf();
    rootfs.join(relative)
}

fn normalize_image_closure(closure: &mut DependencyClosure, rootfs: &Path) {
    let mut prefix = PathBuf::from("payload");
    for component in rootfs.components() {
        if let Component::Normal(part) = component {
            prefix.push(part);
        }
    }
    if prefix == Path::new("payload") {
        return;
    }
    for file in &mut closure.files {
        file.destination = strip_payload_prefix(&file.destination, &prefix);
    }
    for plan in &mut closure.entry_plans {
        plan.binary_destination = strip_payload_prefix(&plan.binary_destination, &prefix);
        plan.linker_destination = strip_payload_prefix(&plan.linker_destination, &prefix);
        plan.library_dirs = plan
            .library_dirs
            .iter()
            .map(|dir| strip_payload_prefix(dir, &prefix))
            .collect();
    }
}

fn strip_payload_prefix(path: &Path, prefix: &Path) -> PathBuf {
    if let Ok(stripped) = path.strip_prefix(prefix) {
        let mut new_path = PathBuf::from("payload");
        new_path.push(stripped);
        new_path
    } else {
        path.to_path_buf()
    }
}

fn resolve_trace_backend(arg: TraceBackendArg) -> Result<Option<TraceBackendKind>> {
    match arg {
        TraceBackendArg::Off => Ok(None),
        TraceBackendArg::Auto => {
            #[cfg(target_os = "linux")]
            {
                Ok(Some(TraceBackendKind::combined()))
            }
            #[cfg(not(target_os = "linux"))]
            {
                Ok(Some(TraceBackendKind::null()))
            }
        }
        TraceBackendArg::Ptrace => {
            #[cfg(target_os = "linux")]
            {
                Ok(Some(TraceBackendKind::ptrace()))
            }
            #[cfg(not(target_os = "linux"))]
            {
                bail!("ptrace backend is only available on Linux");
            }
        }
        TraceBackendArg::Fanotify => {
            #[cfg(target_os = "linux")]
            {
                Ok(Some(TraceBackendKind::fanotify()))
            }
            #[cfg(not(target_os = "linux"))]
            {
                bail!("fanotify backend is only available on Linux");
            }
        }
        TraceBackendArg::Combined => {
            #[cfg(target_os = "linux")]
            {
                Ok(Some(TraceBackendKind::combined()))
            }
            #[cfg(not(target_os = "linux"))]
            {
                bail!("combined trace backend is only available on Linux");
            }
        }
    }
}

fn log_closure_stats(label: &str, closure: &DependencyClosure) {
    if closure.entry_plans.is_empty() {
        debug!("{label}: no entry plans collected");
        return;
    }
    info!(
        "{label}: {} file(s), {} entry plan(s)",
        closure.files.len(),
        closure.entry_plans.len()
    );
    if !closure.traced_files.is_empty() {
        debug!(
            "{label}: trace collector captured {} runtime file(s)",
            closure.traced_files.len()
        );
    }
}

fn log_merge_report(reference: &str, report: &MergeReport) {
    info!(
        "image `{}` merge summary: {} file(s) added, {} reused, {} entry(es) added",
        reference, report.added_files, report.reused_files, report.added_entries
    );
    if !report.conflicts.is_empty() {
        warn!(
            "{} conflict(s) while merging image `{}` into closure",
            report.conflicts.len(),
            reference
        );
        for conflict in report.conflicts.iter().take(3) {
            warn!(
                " - destination {} already provided (existing digest {} vs new {})",
                conflict.destination.display(),
                conflict.existing_digest,
                conflict.incoming_digest,
            );
        }
        if report.conflicts.len() > 3 {
            warn!(
                " - {} additional conflicts suppressed",
                report.conflicts.len() - 3
            );
        }
    }
}

fn parse_image_env(vars: &[String]) -> Vec<(OsString, OsString)> {
    let mut pairs = Vec::new();
    for entry in vars {
        if let Some((key, value)) = entry.split_once('=') {
            pairs.push((OsString::from(key), OsString::from(value)));
        }
    }
    pairs
}

fn log_validation_report(report: &ValidationReport) {
    for entry in &report.entries {
        match entry.status {
            EntryValidationStatus::StaticOk | EntryValidationStatus::DynamicOk { .. } => {
                info!(
                    "validation ok: {} ({})",
                    entry.display_name,
                    describe_status(&entry.status)
                );
            }
            _ => {
                warn!(
                    "validation issue: {} ({})",
                    entry.display_name,
                    describe_status(&entry.status)
                );
            }
        }
    }
}

fn describe_status(status: &EntryValidationStatus) -> String {
    match status {
        EntryValidationStatus::StaticOk => "static entry".to_string(),
        EntryValidationStatus::DynamicOk { resolved } => {
            format!("dynamic entry (resolved {} libs)", resolved)
        }
        EntryValidationStatus::MissingBinary => "binary missing".to_string(),
        EntryValidationStatus::MissingLinker => "linker missing".to_string(),
        EntryValidationStatus::LinkerError { error } => match error {
            LinkerFailure::Spawn { linker, message } => {
                format!("failed to spawn linker {}: {}", linker.display(), message)
            }
            LinkerFailure::CommandFailed {
                linker,
                status,
                stderr,
            } => format!(
                "linker {} exited {:?}: {}",
                linker.display(),
                status,
                stderr
            ),
            LinkerFailure::LibraryNotFound { name, raw } => {
                format!("missing library {} ({})", name, raw)
            }
            LinkerFailure::InvalidPath { path } => {
                format!("invalid path {}", path.display())
            }
            LinkerFailure::Other { message } => message.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_create_cmd_with_host_entries() {
        let cli = Cli::parse_from([
            "sidebundle",
            "create",
            "--from-host",
            "/bin/echo",
            "--log-level",
            "debug",
        ]);
        match cli.command {
            Commands::Create(args) => {
                assert_eq!(args.from_host.len(), 1);
                assert_eq!(args.target, "linux-x86_64");
                assert!(args.from_image.is_empty());
            }
            _ => panic!("unexpected command variant"),
        }
    }

    #[test]
    fn parse_create_cmd_with_image_entries() {
        let cli = Cli::parse_from([
            "sidebundle",
            "create",
            "--from-image",
            "docker://alpine:3.20::/bin/sh",
        ]);
        match cli.command {
            Commands::Create(args) => {
                assert!(args.from_host.is_empty());
                assert_eq!(args.from_image.len(), 1);
            }
            _ => panic!("unexpected command variant"),
        }
    }
}
#[derive(Subcommand)]
enum AgentCommands {
    Trace(AgentTraceArgs),
}
