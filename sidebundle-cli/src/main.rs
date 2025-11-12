use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use env_logger::Env;
use log::{debug, info, warn, LevelFilter};
use sidebundle_closure::{
    image::{DockerProvider, ImageRoot, ImageRootProvider, PodmanProvider},
    trace::TraceCollector,
    validator::BundleValidator,
    ClosureBuilder,
};
use sidebundle_core::{BundleEntry, BundleSpec, DependencyClosure, MergeReport, TargetTriple};
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
        spec.push_entry(BundleEntry::new(path, display));
    }

    let mut builder = ClosureBuilder::new();
    if let Some(root) = &trace_root {
        builder = builder.with_chroot_root(root.clone());
    }
    if trace_mode != TraceMode::Off {
        let mut tracer = TraceCollector::new();
        if let Some(root) = &trace_root {
            tracer = tracer.with_root(root.clone());
        }
        builder = builder.with_tracer(tracer);
    }

    let mut closure = builder
        .build(&spec)
        .context("failed to build dependency closure")?;
    log_closure_stats("host inputs", &closure);

    let default_backend = BackendPreference::from(image_backend);
    let grouped = group_image_entries(&from_image, default_backend)?;
    for ((preference, reference), paths) in grouped {
        info!(
            "constructing closure for image `{}` via {:?} backend ({} entr{} )",
            reference,
            preference,
            paths.len(),
            if paths.len() == 1 { "y" } else { "ies" }
        );
        let image_closure = build_image_closure(preference, &reference, &paths, target, trace_mode)
            .with_context(|| {
                format!("failed to build closure for image `{reference}` using backend {preference:?}")
            })?;
        log_closure_stats(&format!("image `{reference}`"), &image_closure);
        let report = closure.merge(image_closure);
        log_merge_report(&reference, &report);
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
    BundleValidator::new()
        .validate(&output, &closure.entry_plans)
        .context("bundle validation failed")?;
    info!("linker validation succeeded");
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

fn build_image_closure(
    preference: BackendPreference,
    reference: &str,
    paths: &[PathBuf],
    target: TargetTriple,
    trace_mode: TraceMode,
) -> Result<DependencyClosure> {
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
            Ok(closure) => return Ok(closure),
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
) -> Result<DependencyClosure> {
    match backend {
        BackendPreference::Docker => {
            let provider = DockerProvider::new();
            let root = provider
                .prepare_root(reference)
                .with_context(|| "docker provider invocation failed")?;
            build_closure_from_root("docker", reference, target, root, paths, trace_mode)
        }
        BackendPreference::Podman | BackendPreference::Auto => {
            let provider = PodmanProvider::new();
            let root = provider
                .prepare_root(reference)
                .with_context(|| "podman provider invocation failed")?;
            build_closure_from_root("podman", reference, target, root, paths, trace_mode)
        }
    }
}

fn build_closure_from_root(
    backend_name: &'static str,
    reference: &str,
    target: TargetTriple,
    root: ImageRoot,
    entry_paths: &[PathBuf],
    trace_mode: TraceMode,
) -> Result<DependencyClosure> {
    let mut spec = BundleSpec::new(format!("{backend_name}:{reference}"), target);
    for (idx, virtual_path) in entry_paths.iter().enumerate() {
        let physical = physical_image_path(root.rootfs(), virtual_path);
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
        spec.push_entry(BundleEntry::new(physical, display));
    }

    let mut builder = ClosureBuilder::new().with_chroot_root(root.rootfs().to_path_buf());
    if trace_mode != TraceMode::Off {
        let tracer = TraceCollector::new().with_root(root.rootfs().to_path_buf());
        builder = builder.with_tracer(tracer);
    }

    let mut closure = builder
        .build(&spec)
        .with_context(|| format!("failed to build closure inside image `{reference}`"))?;
    normalize_image_closure(&mut closure, root.rootfs());
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
            warn!(" - {} additional conflicts suppressed", report.conflicts.len() - 3);
        }
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
        }
    }
}
