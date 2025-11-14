use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use env_logger::Env;
use log::{debug, info, warn, LevelFilter};
use serde::Deserialize;
use sidebundle_closure::{
    image::{DockerProvider, ImageRoot, ImageRootProvider, PodmanProvider},
    trace::{TraceBackendKind, TraceCollector},
    validator::{BundleValidator, EntryValidationStatus, LinkerFailure, ValidationReport},
    ClosureBuilder,
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
        trace_mode,
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
    for (idx, path) in from_host.iter().enumerate() {
        std::fs::metadata(path)
            .with_context(|| format!("failed to read host executable: {}", path.display()))?;
        let display = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_owned())
            .unwrap_or_else(|| format!("entry-{idx}"));
        spec.push_entry(BundleSpec::host_entry(path, display));
    }

    let mut builder = ClosureBuilder::new();
    if let Some(root) = &trace_root {
        builder = builder.with_chroot_root(root.clone(), Origin::Host);
    }
    if trace_mode != TraceMode::Off {
        let tracer = TraceCollector::new().with_backend(TraceBackendKind::combined());
        builder = builder.with_tracer(tracer);
    }

    let mut closure = builder
        .build(&spec)
        .context("failed to build dependency closure")?;
    log_closure_stats("host inputs", &closure);

    let default_backend = BackendPreference::from(image_backend);
    let grouped = group_image_entries(&from_image, default_backend)?;
    let mut image_guards = Vec::new();
    for ((preference, reference), paths) in grouped {
        info!(
            "constructing closure for image `{}` via {:?} backend ({} entr{} )",
            reference,
            preference,
            paths.len(),
            if paths.len() == 1 { "y" } else { "ies" }
        );
        let image_result = build_image_closure(preference, &reference, &paths, target, trace_mode)
            .with_context(|| {
                format!(
                    "failed to build closure for image `{reference}` using backend {preference:?}"
                )
            })?;
        log_closure_stats(&format!("image `{reference}`"), &image_result.closure);
        let report = closure.merge(image_result.closure);
        log_merge_report(&reference, &report);
        image_guards.push(image_result.guard);
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
        spec.push_entry(BundleEntry::new(logical, display));
    }

    let mut builder =
        ClosureBuilder::new().with_chroot_root(args.rootfs.clone(), agent_origin);
    if args.trace_mode != TraceMode::Off {
        let env_pairs = parse_image_env(&agent_spec.env);
        let mut tracer = TraceCollector::new();
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
    /// Executable paths on the host
    #[arg(long = "from-host", value_name = "PATH", num_args = 0..)]
    from_host: Vec<PathBuf>,

    /// Image reference and path pairs (format: [backend://]IMAGE::/path/in/image)
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

    /// Runtime tracing mode
    #[arg(long = "trace-mode", value_enum, default_value_t = TraceMode::Auto)]
    trace_mode: TraceMode,

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

    /// Trace mode
    #[arg(long = "trace-mode", value_enum, default_value_t = TraceMode::Auto)]
    trace_mode: TraceMode,
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
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum TraceMode {
    Off,
    Auto,
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
struct ImageEntryArg {
    backend: Option<BackendPreference>,
    reference: String,
    path: PathBuf,
}

fn parse_image_entry(value: &str) -> Result<ImageEntryArg, String> {
    let (image_part, path_part) = value
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
    Ok(ImageEntryArg {
        backend,
        reference: reference.trim().to_string(),
        path,
    })
}

fn group_image_entries(
    inputs: &[ImageEntryArg],
    default_backend: BackendPreference,
) -> Result<BTreeMap<(BackendPreference, String), Vec<PathBuf>>> {
    let mut map: BTreeMap<(BackendPreference, String), Vec<PathBuf>> = BTreeMap::new();
    for input in inputs {
        let preference = input.backend.unwrap_or(default_backend);
        map.entry((preference, input.reference.clone()))
            .or_default()
            .push(input.path.clone());
    }
    Ok(map)
}

struct ImageClosureResult {
    closure: DependencyClosure,
    guard: ImageRoot,
}

fn build_image_closure(
    preference: BackendPreference,
    reference: &str,
    paths: &[PathBuf],
    target: TargetTriple,
    trace_mode: TraceMode,
) -> Result<ImageClosureResult> {
    if paths.is_empty() {
        bail!("no entry paths provided for image `{reference}`");
    }
    let attempts: Vec<BackendPreference> = match preference {
        BackendPreference::Auto => vec![BackendPreference::Docker, BackendPreference::Podman],
        other => vec![other],
    };
    let mut errors = Vec::new();
    for backend in attempts {
        match build_with_backend(backend, reference, paths, target, trace_mode) {
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
    paths: &[PathBuf],
    target: TargetTriple,
    trace_mode: TraceMode,
) -> Result<ImageClosureResult> {
    match backend {
        BackendPreference::Docker => {
            let provider = DockerProvider::new();
            let root = provider
                .prepare_root(reference)
                .with_context(|| "docker provider invocation failed")?;
            let closure =
                build_closure_from_root("docker", reference, target, &root, paths, trace_mode)?;
            Ok(ImageClosureResult {
                closure,
                guard: root,
            })
        }
        BackendPreference::Podman | BackendPreference::Auto => {
            let provider = PodmanProvider::new();
            let root = provider
                .prepare_root(reference)
                .with_context(|| "podman provider invocation failed")?;
            let closure =
                build_closure_from_root("podman", reference, target, &root, paths, trace_mode)?;
            Ok(ImageClosureResult {
                closure,
                guard: root,
            })
        }
    }
}

fn build_closure_from_root(
    backend_name: &'static str,
    reference: &str,
    target: TargetTriple,
    root: &ImageRoot,
    entry_paths: &[PathBuf],
    trace_mode: TraceMode,
) -> Result<DependencyClosure> {
    let rootfs = root.rootfs();
    let image_env = parse_image_env(&root.config().env);
    let mut spec = BundleSpec::new(format!("{backend_name}:{reference}"), target);
    let origin = Origin::Image(reference.to_string());
    for (idx, virtual_path) in entry_paths.iter().enumerate() {
        let physical = physical_image_path(rootfs, virtual_path);
        std::fs::metadata(&physical).with_context(|| {
            format!(
                "image `{reference}` missing executable {} (resolved path {})",
                virtual_path.display(),
                physical.display()
            )
        })?;
        let display = virtual_path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("image-entry-{idx}"));
        let logical = LogicalPath::new(origin.clone(), virtual_path.clone());
        spec.push_entry(BundleEntry::new(logical, display));
    }

    let mut builder = ClosureBuilder::new().with_chroot_root(rootfs.to_path_buf(), origin);
    if trace_mode != TraceMode::Off {
        let mut tracer = TraceCollector::new();
        if !image_env.is_empty() {
            tracer = tracer.with_env(image_env.clone());
        }
        builder = builder.with_tracer(tracer);
    }

    let mut closure = builder
        .build(&spec)
        .with_context(|| format!("failed to build closure inside image `{reference}`"))?;
    normalize_image_closure(&mut closure, rootfs);
    Ok(closure)
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
