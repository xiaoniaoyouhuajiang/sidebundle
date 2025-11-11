use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use env_logger::Env;
use log::{info, LevelFilter};
use sidebundle_closure::ClosureBuilder;
use sidebundle_core::{BundleEntry, BundleSpec, TargetTriple};
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
        name,
        target,
        out_dir,
    } = args;

    let target = TargetTriple::parse(&target)
        .with_context(|| format!("unsupported target triple: {}", target))?;

    info!(
        "building bundle `{}` for target {} with {} host executables",
        name,
        target,
        from_host.len()
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

    let closure = ClosureBuilder::new()
        .build(&spec)
        .context("failed to build dependency closure")?;
    info!(
        "dependency closure built with {} files (including entries)",
        closure.files.len()
    );
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
    #[arg(
        long = "from-host",
        value_name = "PATH",
        required = true,
        num_args = 1..,
    )]
    from_host: Vec<PathBuf>,

    /// Bundle name
    #[arg(long = "name", default_value = "bundle")]
    name: String,

    /// Target triple
    #[arg(long = "target", default_value = "linux-x86_64")]
    target: String,

    /// Output root (defaults to target/bundles)
    #[arg(long = "out-dir", value_name = "DIR")]
    out_dir: Option<PathBuf>,
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
            }
        }
    }
}
