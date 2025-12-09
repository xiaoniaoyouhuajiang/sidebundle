use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::env;
use std::ffi::{CStr, OsString};
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{ErrorKind, Read};
use std::mem::MaybeUninit;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use env_logger::Env;
use log::{debug, info, warn, LevelFilter};
use pathdiff::diff_paths;
use sha2::{Digest, Sha256};
use sidebundle_closure::{
    image::{DockerProvider, ImageRoot, ImageRootProvider, PodmanProvider},
    trace::{
        TraceBackendKind, TraceCollector, TraceCommand as RuntimeTraceCommand, TraceSpec,
        TraceSpecReport, TRACE_REPORT_VERSION,
    },
    validator::{BundleValidator, EntryValidationStatus, LinkerFailure, ValidationReport},
    ChrootPathResolver, ClosureBuilder, PathResolver, ResolverSet,
};
use sidebundle_core::{
    AuxvEntry, BundleEntry, BundleSpec, DependencyClosure, LogicalPath, MergeReport, Origin,
    ResolvedFile, RunMode, RuntimeMetadata, SystemInfo, TargetTriple,
};
use sidebundle_packager::Packager;

mod agent;
use agent::{AgentLaunchConfig, AgentRunResult, AgentTraceRunner};

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
        image_agent_bin,
        image_agent_cli,
        image_agent_keep_output,
        image_agent_keep_rootfs,
        copy_dir,
        allow_gpu_libs,
        strict_validate,
        set_env,
        run_mode,
        emit_shim,
    } = args;

    if from_host.is_empty() && from_image.is_empty() && copy_dir.is_empty() {
        bail!("at least one --from-host, --from-image or --copy-dir entry is required");
    }

    let target = TargetTriple::parse(&target)
        .with_context(|| format!("unsupported target triple: {target}"))?;

    info!(
        "building bundle `{}` for target {} ({} host entries, {} image entries)",
        name,
        target,
        from_host.len(),
        from_image.len()
    );

    let run_mode_resolved = resolve_run_mode(run_mode);
    let mut spec = BundleSpec::new(name, target);
    spec.set_run_mode(run_mode_resolved);
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
    let agent_launch = if matches!(
        image_backend_choice,
        TraceBackendArg::Agent | TraceBackendArg::AgentCombined
    ) {
        Some(
            AgentLaunchConfig::from_args(
                image_agent_bin,
                image_agent_cli,
                image_agent_keep_output,
                image_agent_keep_rootfs,
            )
            .context("failed to configure image agent settings")?,
        )
    } else {
        None
    };

    let mut host_resolvers = ResolverSet::new();
    if let Some(root) = &trace_root {
        let resolver = Arc::new(ChrootPathResolver::from_root(root.clone(), Origin::Host));
        host_resolvers.insert(Origin::Host, resolver);
    }
    let mut builder = ClosureBuilder::new()
        .with_resolver_set(host_resolvers.clone())
        .with_allow_gpu_libs(allow_gpu_libs);
    if let Some(backend) = host_backend.clone() {
        let tracer = TraceCollector::new().with_backend(backend);
        builder = builder.with_tracer(tracer);
    }

    let mut closure = builder
        .build(&spec)
        .context("failed to build dependency closure")?;
    log_closure_stats("host inputs", &closure);
    let mut resolver_entries: Vec<(Origin, Arc<dyn PathResolver>)> = Vec::new();
    if let Some(resolver) = host_resolvers.get(&Origin::Host) {
        resolver_entries.push((Origin::Host, resolver));
    }

    let default_backend = BackendPreference::from(image_backend);
    let grouped = group_image_entries(&from_image, default_backend)?;
    for ((preference, reference), entries) in grouped {
        info!(
            "constructing closure for image `{}` via {:?} backend ({} entr{} )",
            reference,
            preference,
            entries.len(),
            if entries.len() == 1 { "y" } else { "ies" }
        );
        let image_result = build_image_closure(BuildImageArgs {
            preference,
            reference: &reference,
            entries: &entries,
            target,
            trace_backend: image_backend_choice,
            agent_launch: agent_launch.as_ref(),
            allow_gpu_libs,
            run_mode: run_mode_resolved,
        })
        .with_context(|| {
            format!("failed to build closure for image `{reference}` using backend {preference:?}")
        })?;
        log_closure_stats(&format!("image `{reference}`"), &image_result.closure);
        let report = closure.merge(image_result.closure);
        log_merge_report(&reference, &report);
        resolver_entries.extend(image_result.resolver_entries);
    }

    ensure_system_assets(&mut closure, &resolver_entries)
        .context("failed to backfill system assets")?;
    add_copy_dirs(&mut closure, &copy_dir, &resolver_entries)
        .context("failed to apply --copy-dir entries")?;
    apply_env_overrides(&mut closure, &set_env);

    if closure.entry_plans.is_empty() {
        bail!("no executable entries were collected from host or image inputs");
    }

    let packager = if let Some(dir) = out_dir {
        Packager::new()
            .with_output_root(dir)
            .with_shim_output(emit_shim)
    } else {
        Packager::new().with_shim_output(emit_shim)
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
    let trace_spec: TraceSpec =
        serde_json::from_slice(&spec_data).with_context(|| "failed to parse trace spec json")?;
    if trace_spec.commands.is_empty() {
        bail!("agent: trace spec did not contain any commands");
    }
    let runtime_metadata =
        capture_runtime_metadata().context("agent: failed to capture runtime metadata")?;

    let backend =
        resolve_trace_backend(args.trace_backend).context("failed to configure trace backend")?;
    let mut tracer = if let Some(kind) = backend {
        TraceCollector::new().with_backend(kind)
    } else {
        TraceCollector::new()
    };
    if !trace_spec.env.is_empty() {
        let env_pairs = trace_spec
            .env
            .iter()
            .map(|(key, value)| (OsString::from(key), OsString::from(value)))
            .collect();
        tracer = tracer.with_env(env_pairs);
    }

    let resolver = Arc::new(ChrootPathResolver::from_root(
        args.rootfs.clone(),
        Origin::Host,
    ));
    let mut files = BTreeSet::new();
    for command in &trace_spec.commands {
        let Some(program) = command.argv.first() else {
            continue;
        };
        let logical = LogicalPath::new(Origin::Host, PathBuf::from(program));
        let mut trace_command = RuntimeTraceCommand::new(logical);
        if command.argv.len() > 1 {
            trace_command = trace_command.with_args(command.argv[1..].to_vec());
        }
        let artifacts = tracer
            .run(resolver.as_ref(), &trace_command)
            .with_context(|| format!("agent: trace invocation failed for {program}"))?;
        for artifact in artifacts {
            files.insert(artifact.runtime_path);
        }
    }

    let report = TraceSpecReport {
        schema_version: TRACE_REPORT_VERSION,
        files: files.into_iter().collect(),
        metadata: Some(runtime_metadata),
    };
    let report_path = args.output.join("report.json");
    let tmp_path = report_path.with_extension("tmp");
    let data = serde_json::to_vec_pretty(&report).context("agent: failed to serialize report")?;
    fs::write(&tmp_path, data).with_context(|| {
        format!(
            "agent: failed to write report temp file {}",
            tmp_path.display()
        )
    })?;
    fs::rename(&tmp_path, &report_path).with_context(|| {
        format!(
            "agent: failed to finalize report file {}",
            report_path.display()
        )
    })?;
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

    /// Recursively copy a host directory into the bundle payload (format: SRC[:DEST])
    #[arg(long = "copy-dir", value_name = "SRC[:DEST]", value_parser = parse_copy_dir, num_args = 0..)]
    copy_dir: Vec<CopyDirArg>,

    /// Allow GPU/DRM libraries (e.g., libdrm, libnvidia) to be included in the bundle
    #[arg(long = "allow-gpu-libs")]
    allow_gpu_libs: bool,

    /// Emit self-extracting shim executables alongside the bundle
    #[arg(long = "emit-shim")]
    emit_shim: bool,

    /// Override env vars in generated launchers (KEY=VALUE, repeatable)
    #[arg(
        long = "set-env",
        value_name = "KEY=VALUE",
        value_parser = parse_kv,
        num_args = 0..
    )]
    set_env: Vec<(String, String)>,

    /// Runtime execution mode for launchers
    #[arg(long = "run-mode", value_enum, default_value_t = RunModeArg::Host)]
    run_mode: RunModeArg,

    /// Runtime trace backend for host inputs
    #[arg(long = "trace-backend", value_enum, default_value_t = TraceBackendArg::Auto)]
    trace_backend: TraceBackendArg,

    /// Runtime trace backend for image inputs
    #[arg(long = "image-trace-backend", value_enum)]
    image_trace_backend: Option<TraceBackendArg>,

    /// Path to the sidebundle agent binary to mount into containers
    #[arg(long = "image-agent-bin", value_name = "PATH")]
    image_agent_bin: Option<PathBuf>,

    /// Override container engine command used for agent runs (e.g. "sudo -n docker")
    #[arg(long = "image-agent-cli", value_name = "CMD")]
    image_agent_cli: Option<String>,

    /// Preserve agent output directories for debugging
    #[arg(long = "image-agent-keep-output")]
    image_agent_keep_output: bool,

    /// Preserve the exported agent rootfs directory for debugging
    #[arg(long = "image-agent-keep-rootfs")]
    image_agent_keep_rootfs: bool,

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

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum TraceBackendArg {
    Off,
    Auto,
    Ptrace,
    Fanotify,
    Combined,
    Agent,
    AgentCombined,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum ImageBackendArg {
    Auto,
    Docker,
    Podman,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum RunModeArg {
    Host,
    Bwrap,
    Chroot,
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
pub(crate) enum BackendPreference {
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
pub(crate) struct ImageEntryArg {
    backend: Option<BackendPreference>,
    reference: String,
    path: PathBuf,
    trace_args: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
enum CopyDirArg {
    Host {
        source: PathBuf,
        dest: PathBuf,
    },
    Image {
        backend: Option<BackendPreference>,
        reference: String,
        path: PathBuf,
        dest: PathBuf,
    },
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

fn parse_copy_dir(value: &str) -> Result<CopyDirArg, String> {
    // Image form: [backend://]IMAGE::/abs/path[:DEST]
    if let Some((image_part, rest)) = value.split_once("::") {
        let path_dest = rest;
        let (path_part, dest_part) = match path_dest.split_once(':') {
            Some((p, d)) if !d.is_empty() => (p, Some(d)),
            _ => (path_dest, None),
        };
        let path_part = path_part.trim();
        if path_part.is_empty() {
            return Err("copy-dir image path cannot be empty".into());
        }
        let path = PathBuf::from(path_part);
        if !path.is_absolute() {
            return Err("copy-dir image path must be absolute".into());
        }
        let (backend, reference) = if let Some(rest) = image_part.strip_prefix("docker://") {
            (Some(BackendPreference::Docker), rest.to_string())
        } else if let Some(rest) = image_part.strip_prefix("podman://") {
            (Some(BackendPreference::Podman), rest.to_string())
        } else {
            (None, image_part.to_string())
        };
        if reference.trim().is_empty() {
            return Err("copy-dir image reference cannot be empty".into());
        }
        let dest = dest_part
            .map(|d| PathBuf::from(d.trim()))
            .unwrap_or_else(|| path.clone());
        return Ok(CopyDirArg::Image {
            backend,
            reference: reference.trim().to_string(),
            path,
            dest,
        });
    }

    // Host form: SRC[:DEST]
    let (src, dest) = match value.split_once(':') {
        Some((s, d)) if !d.is_empty() => (s, Some(d)),
        _ => (value, None),
    };
    let src = src.trim();
    if src.is_empty() {
        return Err("copy-dir source cannot be empty".into());
    }
    let source = PathBuf::from(src);
    let dest = dest
        .map(|d| PathBuf::from(d.trim()))
        .unwrap_or_else(|| PathBuf::from(src.trim()));
    Ok(CopyDirArg::Host { source, dest })
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

fn parse_kv(value: &str) -> Result<(String, String), String> {
    let (k, v) = value
        .split_once('=')
        .ok_or_else(|| "expected KEY=VALUE".to_string())?;
    if k.trim().is_empty() {
        return Err("env key cannot be empty".into());
    }
    Ok((k.to_string(), v.to_string()))
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

fn ensure_system_assets(
    closure: &mut DependencyClosure,
    resolvers: &[(Origin, Arc<dyn PathResolver>)],
) -> Result<()> {
    let mut existing: HashSet<PathBuf> = HashSet::new();
    for file in &closure.files {
        mark_existing(&mut existing, &file.destination);
    }
    for (origin, resolver) in resolvers {
        if !matches!(origin, Origin::Image(_)) {
            continue;
        }
        for asset in SYSTEM_ASSET_PATHS {
            let logical = LogicalPath::new(origin.clone(), PathBuf::from(asset));
            let candidate = resolver.to_host(&logical);
            match fs::metadata(&candidate) {
                Ok(meta) if meta.is_file() => {}
                _ => continue,
            }
            let destination = payload_path_for(logical.path());
            if existing.contains(&destination) {
                continue;
            }
            let digest = compute_digest(&candidate).with_context(|| {
                format!(
                    "failed to hash system asset {} (origin {:?})",
                    candidate.display(),
                    origin
                )
            })?;
            debug!(
                "system asset {} resolved to {} for {:?}",
                asset,
                candidate.display(),
                origin
            );
            closure.files.push(ResolvedFile {
                source: candidate.clone(),
                destination: destination.clone(),
                digest,
            });
            mark_existing(&mut existing, &destination);
        }
    }
    Ok(())
}

fn add_copy_dirs(
    closure: &mut DependencyClosure,
    dirs: &[CopyDirArg],
    resolvers: &[(Origin, Arc<dyn PathResolver>)],
) -> Result<()> {
    for entry in dirs {
        match entry {
            CopyDirArg::Host { source, dest } => {
                if !source.is_dir() {
                    bail!(
                        "--copy-dir source must be a directory: {}",
                        source.display()
                    );
                }
                copy_dir_into_closure(closure, source, dest)?;
            }
            CopyDirArg::Image {
                backend,
                reference,
                path,
                dest,
            } => {
                // Try to reuse existing resolver/rootfs for this image first.
                let logical = LogicalPath::new(Origin::Image(reference.clone()), path.clone());
                let physical = resolvers
                    .iter()
                    .find(|(origin, _)| origin == logical.origin())
                    .map(|(_, res)| res.to_host(&logical));

                let physical = if let Some(p) = physical {
                    p
                } else {
                    let provider_pref = backend.unwrap_or(BackendPreference::Auto);
                    let provider: Box<dyn ImageRootProvider> = match provider_pref {
                        BackendPreference::Docker | BackendPreference::Auto => {
                            Box::new(DockerProvider::new())
                        }
                        BackendPreference::Podman => Box::new(PodmanProvider::new()),
                    };
                    let image_root = provider
                        .prepare_root(reference)
                        .with_context(|| format!("failed to prepare image root for {reference}"))?;
                    let (_r, rootfs_path, _config) = image_root.into_parts();
                    physical_image_path(&rootfs_path, path)
                };
                if !physical.is_dir() {
                    bail!(
                        "--copy-dir image path must be a directory: {} (in {})",
                        path.display(),
                        reference
                    );
                }
                copy_dir_into_closure(closure, &physical, dest)?;
            }
        }
    }
    Ok(())
}

fn copy_dir_into_closure(
    closure: &mut DependencyClosure,
    source: &Path,
    dest_root: &Path,
) -> Result<()> {
    use walkdir::WalkDir;

    for dirent in WalkDir::new(source).follow_links(false).into_iter() {
        let dirent = dirent.with_context(|| format!("failed to walk {}", source.display()))?;
        let meta = dirent
            .metadata()
            .with_context(|| format!("failed to stat {}", dirent.path().display()))?;
        if meta.is_dir() {
            continue;
        }
        if meta.file_type().is_symlink() {
            debug!(
                "copy-dir: skipping symlink {} (not yet supported)",
                dirent.path().display()
            );
            continue;
        }
        let rel = dirent.path().strip_prefix(source).with_context(|| {
            format!(
                "failed to strip {} prefix from {}",
                source.display(),
                dirent.path().display()
            )
        })?;
        let mut dest = PathBuf::from("payload");
        if dest_root.is_absolute() {
            for comp in dest_root.components().skip(1) {
                dest.push(comp.as_os_str());
            }
        } else {
            dest.push(dest_root);
        }
        dest.push(rel);
        if closure.files.iter().any(|f| f.destination == dest) {
            continue;
        }
        let digest = compute_digest(dirent.path())?;
        closure
            .files
            .push(ResolvedFile::new(dirent.path(), dest, digest));
    }
    Ok(())
}

fn payload_path_for(path: &Path) -> PathBuf {
    let mut dest = PathBuf::from("payload");
    if path.is_absolute() {
        for component in path.components().skip(1) {
            dest.push(component.as_os_str());
        }
    } else {
        dest.push(path);
    }
    dest
}

fn compute_digest(path: &Path) -> Result<String> {
    let mut file = File::open(path)
        .with_context(|| format!("failed to open {} for hashing", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn mark_existing(existing: &mut HashSet<PathBuf>, path: &Path) {
    let mut current = path.to_path_buf();
    loop {
        if existing.contains(&current) {
            // still walk up to avoid missing parents
        } else {
            existing.insert(current.clone());
        }
        if !current.pop() {
            break;
        }
    }
}

struct ImageClosureResult {
    closure: DependencyClosure,
    resolver_entries: Vec<(Origin, Arc<dyn PathResolver>)>,
}

const SYSTEM_ASSET_PATHS: &[&str] = &[
    "/etc/ld.so.cache",
    "/etc/passwd",
    "/etc/group",
    "/etc/nsswitch.conf",
    "/etc/resolv.conf",
    "/etc/hosts",
];

struct BuildImageArgs<'a> {
    preference: BackendPreference,
    reference: &'a str,
    entries: &'a [ImageEntryArg],
    target: TargetTriple,
    trace_backend: TraceBackendArg,
    agent_launch: Option<&'a AgentLaunchConfig>,
    allow_gpu_libs: bool,
    run_mode: RunMode,
}

struct BuildWithBackendArgs<'a> {
    backend: BackendPreference,
    reference: &'a str,
    entries: &'a [ImageEntryArg],
    target: TargetTriple,
    trace_backend: TraceBackendArg,
    agent_launch: Option<&'a AgentLaunchConfig>,
    allow_gpu_libs: bool,
    run_mode: RunMode,
}

struct BuildAgentImageArgs<'a> {
    backend: BackendPreference,
    reference: &'a str,
    entries: &'a [ImageEntryArg],
    target: TargetTriple,
    launch: &'a AgentLaunchConfig,
    trace_backend: TraceBackendArg,
    allow_gpu_libs: bool,
    run_mode: RunMode,
}

struct BuildFromRootArgs<'a> {
    backend: BackendPreference,
    backend_name: &'static str,
    reference: &'a str,
    target: TargetTriple,
    root: &'a ImageRoot,
    entries: &'a [ImageEntryArg],
    trace_backend: TraceBackendArg,
    agent_launch: Option<&'a AgentLaunchConfig>,
    allow_gpu_libs: bool,
    run_mode: RunMode,
    external_traces: Option<Vec<PathBuf>>,
    metadata: Option<RuntimeMetadata>,
}

fn build_image_closure(args: BuildImageArgs) -> Result<ImageClosureResult> {
    let BuildImageArgs {
        preference,
        reference,
        entries,
        target,
        trace_backend,
        agent_launch,
        allow_gpu_libs,
        run_mode,
    } = args;
    if entries.is_empty() {
        bail!("no entry paths provided for image `{reference}`");
    }
    if matches!(
        trace_backend,
        TraceBackendArg::Agent | TraceBackendArg::AgentCombined
    ) {
        let launch = agent_launch
            .ok_or_else(|| anyhow::anyhow!("agent backend requires --image-agent-* options"))?;
        let attempts: Vec<BackendPreference> = match preference {
            BackendPreference::Auto => vec![BackendPreference::Docker, BackendPreference::Podman],
            other => vec![other],
        };
        let mut errors = Vec::new();
        for backend in attempts {
            match build_agent_image_closure(BuildAgentImageArgs {
                backend,
                reference,
                entries,
                target,
                launch,
                trace_backend,
                allow_gpu_libs,
                run_mode,
            }) {
                Ok(result) => return Ok(result),
                Err(err) => errors.push(format!("{backend:?}: {err:?}")),
            }
        }
        bail!(
            "agent backend failed for `{reference}` via all runtimes:\n{}",
            errors.join("\n")
        );
    }

    let attempts: Vec<BackendPreference> = match preference {
        BackendPreference::Auto => vec![BackendPreference::Docker, BackendPreference::Podman],
        other => vec![other],
    };
    let mut errors = Vec::new();
    for backend in attempts {
        match build_with_backend(BuildWithBackendArgs {
            backend,
            reference,
            entries,
            target,
            trace_backend,
            agent_launch,
            allow_gpu_libs,
            run_mode,
        }) {
            Ok(result) => return Ok(result),
            Err(err) => errors.push(format!("{backend:?}: {err:?}")),
        }
    }
    bail!(
        "all image providers failed for `{reference}`:\n{}",
        errors.join("\n")
    );
}

fn build_with_backend(args: BuildWithBackendArgs) -> Result<ImageClosureResult> {
    let BuildWithBackendArgs {
        backend,
        reference,
        entries,
        target,
        trace_backend,
        agent_launch,
        allow_gpu_libs,
        run_mode,
    } = args;
    match backend {
        BackendPreference::Docker => {
            let provider = DockerProvider::new();
            let root = provider
                .prepare_root(reference)
                .with_context(|| "docker provider invocation failed")?;
            build_closure_from_root(BuildFromRootArgs {
                backend,
                backend_name: "docker",
                reference,
                target,
                root: &root,
                entries,
                trace_backend,
                agent_launch,
                allow_gpu_libs,
                run_mode,
                external_traces: None,
                metadata: None,
            })
        }
        BackendPreference::Podman | BackendPreference::Auto => {
            let provider = PodmanProvider::new();
            let root = provider
                .prepare_root(reference)
                .with_context(|| "podman provider invocation failed")?;
            build_closure_from_root(BuildFromRootArgs {
                backend,
                backend_name: "podman",
                reference,
                target,
                root: &root,
                entries,
                trace_backend,
                agent_launch,
                allow_gpu_libs,
                run_mode,
                external_traces: None,
                metadata: None,
            })
        }
    }
}

fn build_agent_image_closure(args: BuildAgentImageArgs) -> Result<ImageClosureResult> {
    let BuildAgentImageArgs {
        backend,
        reference,
        entries,
        target,
        launch,
        trace_backend,
        allow_gpu_libs,
        run_mode,
    } = args;
    let backend_name = match backend {
        BackendPreference::Docker => "docker",
        BackendPreference::Podman => "podman",
        BackendPreference::Auto => unreachable!("auto preference should be resolved earlier"),
    };
    let runner = AgentTraceRunner::new(
        launch.command_for_backend(backend),
        launch.bin_path.clone(),
        launch.keep_output,
        launch.keep_rootfs,
    )?;
    debug!("agent: launching container backed by {backend:?} for `{reference}`");
    let run_result = runner.run(reference, entries, trace_backend)?;
    let AgentRunResult {
        report,
        rootfs,
        config,
    } = run_result;
    let TraceSpecReport {
        schema_version: _,
        files: trace_files,
        metadata,
    } = report;
    debug!(
        "agent: trace for `{}` captured {} file(s); exported rootfs at {}",
        reference,
        trace_files.len(),
        rootfs.path().display()
    );
    for entry in entries {
        let host_path = physical_image_path(rootfs.path(), &entry.path);
        if let Err(err) = std::fs::metadata(&host_path) {
            warn!(
                "agent: entry {} missing from exported rootfs at {} ({err})",
                entry.path.display(),
                host_path.display()
            );
        } else {
            debug!(
                "agent: entry {} resolved to {}",
                entry.path.display(),
                host_path.display()
            );
        }
    }
    let (export_path, cleanup_guard) = rootfs.into_parts();
    let mut image_root = ImageRoot::new(reference, export_path, config);
    if let Some(guard) = cleanup_guard {
        image_root = image_root.with_cleanup(move || drop(guard));
    }
    build_closure_from_root(BuildFromRootArgs {
        backend,
        backend_name,
        reference,
        target,
        root: &image_root,
        entries,
        trace_backend: TraceBackendArg::Off,
        agent_launch: None,
        allow_gpu_libs,
        run_mode,
        external_traces: Some(trace_files),
        metadata,
    })
}

fn build_closure_from_root(args: BuildFromRootArgs) -> Result<ImageClosureResult> {
    let BuildFromRootArgs {
        backend,
        backend_name,
        reference,
        target,
        root,
        entries,
        trace_backend,
        agent_launch,
        allow_gpu_libs,
        run_mode,
        external_traces,
        metadata,
    } = args;
    let rootfs = root.rootfs().to_path_buf();
    ensure_image_library_aliases(&rootfs);
    let image_env = parse_image_env(&root.config().env);
    let mut spec = BundleSpec::new(format!("{backend_name}:{reference}"), target);
    spec.set_run_mode(run_mode);
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
    resolvers.insert(origin.clone(), chroot_resolver.clone());

    let mut builder = ClosureBuilder::new()
        .with_resolver_set(resolvers.clone())
        .with_allow_gpu_libs(allow_gpu_libs);
    if let Some(paths) = external_traces {
        builder = builder.with_external_trace_paths(origin.clone(), paths);
    }
    if let Some(backend) =
        configure_image_trace_backend(trace_backend, backend, reference, agent_launch)?
    {
        let mut tracer = TraceCollector::new().with_backend(backend);
        if !image_env.is_empty() {
            tracer = tracer.with_env(image_env.clone());
        }
        builder = builder.with_tracer(tracer);
    }
    // Merge PATH from image config env and runtime metadata (if present) for command resolution.
    let mut merged_paths: Vec<PathBuf> = Vec::new();
    if let Some(path_value) = image_env
        .iter()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| v.clone())
    {
        merged_paths.extend(ClosureBuilder::split_paths(&path_value.to_string_lossy()));
    }
    if let Some(meta_path) = metadata.as_ref().and_then(|m| m.env.get("PATH")) {
        merged_paths.extend(ClosureBuilder::split_paths(meta_path));
    }
    if !merged_paths.is_empty() {
        // Dedup while preserving order.
        let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        merged_paths.retain(|p| seen.insert(p.clone()));
        builder = builder.with_origin_path(origin.clone(), merged_paths);
    }

    let mut closure = builder
        .build(&spec)
        .with_context(|| format!("failed to build closure inside image `{reference}`"))?;
    if let Some(ref snapshot) = metadata {
        if !snapshot.is_empty() {
            closure.metadata.insert(origin.clone(), snapshot.clone());
        }
    }
    include_java_runtime(root.rootfs(), &origin, &mut closure, metadata.as_ref());
    Ok(ImageClosureResult {
        closure,
        resolver_entries: vec![(origin, chroot_resolver)],
    })
}

fn ensure_image_library_aliases(rootfs: &Path) {
    #[cfg(not(unix))]
    {
        let _ = rootfs;
        return;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        const CANDIDATES: &[(&str, &str)] = &[
            ("/lib64", "/lib/x86_64-linux-gnu"),
            ("/usr/lib64", "/usr/lib/x86_64-linux-gnu"),
        ];

        for (alias, target) in CANDIDATES {
            let alias_rel = alias.trim_start_matches('/');
            let target_rel = target.trim_start_matches('/');
            if alias_rel.is_empty() || target_rel.is_empty() {
                continue;
            }
            debug!(
                "image root {} evaluating alias {} -> {}",
                rootfs.display(),
                alias,
                target
            );
            let alias_path = rootfs.join(alias_rel);
            let target_path = rootfs.join(target_rel);
            if !target_path.exists() {
                debug!(
                    "image root {} missing target {} for alias {}",
                    rootfs.display(),
                    target_rel,
                    alias
                );
                continue;
            }
            if let Some(parent) = alias_path.parent() {
                if let Err(err) = fs::create_dir_all(parent) {
                    warn!(
                        "failed to prepare parent {} for alias {}: {err}",
                        parent.display(),
                        alias
                    );
                    continue;
                }
            }
            let relative_target = alias_path
                .parent()
                .and_then(|parent| diff_paths(&target_path, parent))
                .unwrap_or(target_path.clone());
            if alias_path.exists() {
                match fs::remove_file(&alias_path) {
                    Ok(()) => {
                        debug!(
                            "image root {} removed existing alias target {}",
                            rootfs.display(),
                            alias_path.display()
                        );
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::IsADirectory => {
                        debug!(
                            "image root {} removing directory alias target {}",
                            rootfs.display(),
                            alias_path.display()
                        );
                        if let Err(dir_err) = fs::remove_dir_all(&alias_path) {
                            warn!(
                                "image root {} failed to remove existing {}: {dir_err}",
                                rootfs.display(),
                                alias_path.display()
                            );
                            continue;
                        }
                    }
                    Err(err) => {
                        warn!(
                            "image root {} failed to remove existing {}: {err}",
                            rootfs.display(),
                            alias_path.display()
                        );
                        continue;
                    }
                }
            }
            match symlink(&relative_target, &alias_path) {
                Ok(()) => debug!(
                    "image root {} created alias {} -> {}",
                    rootfs.display(),
                    alias,
                    target
                ),
                Err(err) => warn!(
                    "failed to create alias {} -> {} inside {}: {err}",
                    alias,
                    target,
                    rootfs.display()
                ),
            }
        }
    }
}

fn physical_image_path(rootfs: &Path, virtual_path: &Path) -> PathBuf {
    let relative = virtual_path
        .strip_prefix("/")
        .unwrap_or(virtual_path)
        .to_path_buf();
    rootfs.join(relative)
}

fn configure_image_trace_backend(
    arg: TraceBackendArg,
    _backend: BackendPreference,
    _reference: &str,
    agent_launch: Option<&AgentLaunchConfig>,
) -> Result<Option<TraceBackendKind>> {
    match arg {
        TraceBackendArg::Agent => {
            let _ = agent_launch.ok_or_else(|| {
                anyhow::anyhow!("--image-agent-bin/cli must be provided when using agent backend")
            })?;
            Ok(None)
        }
        TraceBackendArg::AgentCombined => {
            let _ = agent_launch.ok_or_else(|| {
                anyhow::anyhow!("--image-agent-bin/cli must be provided when using agent backend")
            })?;
            Ok(None)
        }
        other => resolve_trace_backend(other),
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
        TraceBackendArg::Agent => {
            bail!("agent trace backend is only available for image inputs");
        }
        TraceBackendArg::AgentCombined => {
            bail!("agent-combined trace backend is only available for image inputs");
        }
    }
}

fn resolve_run_mode(arg: RunModeArg) -> RunMode {
    match arg {
        RunModeArg::Host => RunMode::Host,
        RunModeArg::Bwrap => RunMode::Bwrap,
        RunModeArg::Chroot => RunMode::Chroot,
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
            EntryValidationStatus::StaticOk
            | EntryValidationStatus::DynamicOk { .. }
            | EntryValidationStatus::LinkerSkipped { .. } => {
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

fn apply_env_overrides(closure: &mut DependencyClosure, overrides: &[(String, String)]) {
    if overrides.is_empty() {
        return;
    }
    let overrides: BTreeMap<String, String> = overrides.iter().cloned().collect();
    for snapshot in closure.metadata.values_mut() {
        snapshot.env.extend(overrides.clone());
    }
    for origin in closure.entry_plans.iter().map(|p| p.origin().clone()) {
        closure
            .metadata
            .entry(origin.clone())
            .or_default()
            .env
            .extend(overrides.clone());
    }
}

fn include_java_runtime(
    rootfs: &Path,
    origin: &Origin,
    closure: &mut DependencyClosure,
    metadata: Option<&RuntimeMetadata>,
) {
    let Some(meta) = metadata else {
        return;
    };
    let Some(java_home) = meta.env.get("JAVA_HOME") else {
        return;
    };
    let java_home_path = Path::new(java_home);
    if !java_home_path.is_absolute() {
        return;
    }
    let base = rootfs.join(java_home_path.strip_prefix("/").unwrap_or(java_home_path));
    let logical_base =
        Path::new("/").join(java_home_path.strip_prefix("/").unwrap_or(java_home_path));
    let candidates = [
        (
            logical_base.join("lib/libjava.so"),
            base.join("lib/libjava.so"),
        ),
        (
            logical_base.join("lib/server/libjvm.so"),
            base.join("lib/server/libjvm.so"),
        ),
    ];
    for (logical, host_path) in candidates {
        if !host_path.exists() {
            continue;
        }
        let destination = payload_path_for(&logical);
        if closure.files.iter().any(|f| f.destination == destination) {
            continue;
        }
        match compute_digest(&host_path) {
            Ok(digest) => {
                closure.files.push(ResolvedFile {
                    source: host_path.clone(),
                    destination,
                    digest,
                });
                debug!(
                    "java runtime: added {} from JAVA_HOME {} for origin {:?}",
                    host_path.display(),
                    java_home,
                    origin
                );
            }
            Err(err) => {
                warn!(
                    "java runtime: failed to hash {}: {err}",
                    host_path.display()
                );
            }
        }
    }
}

fn capture_runtime_metadata() -> Result<RuntimeMetadata> {
    let auxv = read_auxv_entries().context("failed to read /proc/self/auxv")?;
    let mut env_map = BTreeMap::new();
    for (key, value) in env::vars() {
        env_map.insert(key, value);
    }
    let uname = match capture_uname() {
        Ok(info) => Some(info),
        Err(err) => {
            debug!("agent: failed to capture uname snapshot: {err}");
            None
        }
    };
    let platform = capture_platform_string();
    let random = capture_random_bytes();
    Ok(RuntimeMetadata {
        auxv,
        env: env_map,
        uname,
        platform,
        random,
    })
}

fn read_auxv_entries() -> Result<Vec<AuxvEntry>> {
    let mut file = File::open("/proc/self/auxv")
        .with_context(|| "failed to open /proc/self/auxv for metadata capture")?;
    let mut entries = Vec::new();
    let mut buf = [0u8; 16];
    loop {
        match file.read_exact(&mut buf) {
            Ok(()) => {
                let mut tag_bytes = [0u8; 8];
                tag_bytes.copy_from_slice(&buf[..8]);
                let tag = u64::from_ne_bytes(tag_bytes);
                let mut value_bytes = [0u8; 8];
                value_bytes.copy_from_slice(&buf[8..]);
                let value = u64::from_ne_bytes(value_bytes);
                if tag == 0 {
                    break;
                }
                entries.push(AuxvEntry { key: tag, value });
            }
            Err(err) if err.kind() == ErrorKind::UnexpectedEof => break,
            Err(err) => {
                return Err(err)
                    .with_context(|| "failed to read /proc/self/auxv while capturing metadata")
            }
        }
    }
    Ok(entries)
}

fn capture_uname() -> Result<SystemInfo> {
    let mut uts = MaybeUninit::<libc::utsname>::zeroed();
    let rc = unsafe { libc::uname(uts.as_mut_ptr()) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error())
            .context("uname call failed while capturing metadata");
    }
    let uts = unsafe { uts.assume_init() };
    Ok(SystemInfo {
        sysname: uts_field_to_string(&uts.sysname),
        nodename: uts_field_to_string(&uts.nodename),
        release: uts_field_to_string(&uts.release),
        version: uts_field_to_string(&uts.version),
        machine: uts_field_to_string(&uts.machine),
    })
}

fn uts_field_to_string(buf: &[libc::c_char]) -> String {
    unsafe { CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned() }
}

fn capture_platform_string() -> Option<String> {
    unsafe {
        let ptr = libc::getauxval(libc::AT_PLATFORM) as *const libc::c_char;
        if ptr.is_null() {
            None
        } else {
            Some(CStr::from_ptr(ptr).to_string_lossy().into_owned())
        }
    }
}

fn capture_random_bytes() -> Option<[u8; 16]> {
    unsafe {
        let ptr = libc::getauxval(libc::AT_RANDOM) as *const u8;
        if ptr.is_null() {
            return None;
        }
        let slice = std::slice::from_raw_parts(ptr, 16);
        let mut buf = [0u8; 16];
        buf.copy_from_slice(slice);
        Some(buf)
    }
}

fn describe_status(status: &EntryValidationStatus) -> String {
    match status {
        EntryValidationStatus::StaticOk => "static entry".to_string(),
        EntryValidationStatus::DynamicOk { resolved } => {
            format!("dynamic entry (resolved {resolved} libs)")
        }
        EntryValidationStatus::LinkerSkipped { reason } => {
            format!("linker validation skipped ({reason})")
        }
        EntryValidationStatus::MissingBinary => "binary missing".to_string(),
        EntryValidationStatus::MissingInterpreter => "interpreter missing".to_string(),
        EntryValidationStatus::MissingLinker => "linker missing".to_string(),
        EntryValidationStatus::LinkerError { error } => match error {
            LinkerFailure::Spawn { linker, message } => {
                format!("failed to spawn linker {}: {message}", linker.display())
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
                format!("missing library {name} ({raw})")
            }
            LinkerFailure::InvalidPath { path } => {
                format!("invalid path {}", path.display())
            }
            LinkerFailure::UnsupportedStub { linker, message } => {
                format!("linker {} unsupported stub: {message}", linker.display())
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
