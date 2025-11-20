use std::env;
use std::ffi::OsString;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use log::{debug, info};
use serde::Deserialize;
use sidebundle_closure::{
    image::ImageConfig,
    trace::{AgentTraceCommand, TraceSpec, TraceSpecReport},
};
use tempfile::TempDir;

use crate::{BackendPreference, ImageEntryArg};

#[derive(Clone)]
pub(crate) struct AgentLaunchConfig {
    pub(crate) bin_path: PathBuf,
    pub(crate) keep_output: bool,
    pub(crate) keep_rootfs: bool,
    cli_override: Option<Vec<OsString>>,
}

impl AgentLaunchConfig {
    pub(crate) fn from_args(
        bin_override: Option<PathBuf>,
        cli_override: Option<String>,
        keep_output: bool,
        keep_rootfs: bool,
    ) -> Result<Self> {
        let mut raw_path = if let Some(path) = bin_override {
            path
        } else {
            env::current_exe().context("failed to locate sidebundle executable")?
        };
        if !raw_path.is_absolute() {
            raw_path = env::current_dir()
                .context("failed to resolve current directory for agent binary lookup")?
                .join(raw_path);
        }
        let bin_path = fs::canonicalize(&raw_path).with_context(|| {
            format!(
                "failed to canonicalize image agent binary at {}",
                raw_path.display()
            )
        })?;
        let cli_override = match cli_override {
            Some(spec) => Some(parse_engine_command(&spec)?),
            None => None,
        };
        Ok(Self {
            bin_path,
            keep_output,
            keep_rootfs,
            cli_override,
        })
    }

    pub(crate) fn command_for_backend(&self, backend: BackendPreference) -> Vec<OsString> {
        if let Some(cmd) = &self.cli_override {
            return cmd.clone();
        }
        match backend {
            BackendPreference::Podman | BackendPreference::Auto => vec![OsString::from("podman")],
            _ => vec![OsString::from("docker")],
        }
    }
}

pub(crate) struct AgentRunResult {
    pub(crate) report: TraceSpecReport,
    pub(crate) rootfs: RootfsExport,
    pub(crate) config: ImageConfig,
}

pub(crate) struct AgentTraceRunner {
    runtime_cmd: Vec<OsString>,
    agent_bin: PathBuf,
    keep_output: bool,
    keep_rootfs: bool,
}

impl AgentTraceRunner {
    pub(crate) fn new(
        cmd: Vec<OsString>,
        agent_bin: PathBuf,
        keep_output: bool,
        keep_rootfs: bool,
    ) -> Result<Self> {
        if cmd.is_empty() {
            bail!("agent runtime command cannot be empty");
        }
        Ok(Self {
            runtime_cmd: cmd,
            agent_bin,
            keep_output,
            keep_rootfs,
        })
    }

    pub(crate) fn run(&self, reference: &str, entries: &[ImageEntryArg]) -> Result<AgentRunResult> {
        let spec_dir = TempDir::new().context("failed to create agent spec dir")?;
        let out_dir = TempDir::new().context("failed to create agent output dir")?;
        let export_dir = TempDir::new().context("failed to create agent rootfs dir")?;

        let container_name = format!(
            "sidebundle-agent-{}-{}",
            sanitize_reference(reference),
            current_millis()
        );
        let mut create_cmd = self.base_command();
        create_cmd
            .arg("create")
            .arg("--name")
            .arg(&container_name)
            .arg("--cap-add")
            .arg("SYS_PTRACE")
            .arg("--security-opt")
            .arg("seccomp=unconfined")
            .arg("--pids-limit")
            .arg("0")
            .arg("--network")
            .arg("none")
            .arg("--entrypoint")
            .arg("/sb/agent")
            .arg("-v")
            .arg(bind_mount_arg(&self.agent_bin, "/sb/agent", "ro"))
            .arg("-v")
            .arg(bind_mount_arg(spec_dir.path(), "/sb-in", "ro"))
            .arg("-v")
            .arg(bind_mount_arg(out_dir.path(), "/sb-out", "rw"))
            .arg(reference)
            .arg("agent")
            .arg("trace")
            .arg("--rootfs")
            .arg("/")
            .arg("--spec")
            .arg("/sb-in/spec.json")
            .arg("--output")
            .arg("/sb-out")
            .arg("--trace-backend")
            .arg("ptrace");
        let create_out = create_cmd
            .output()
            .context("failed to create agent container")?;
        if !create_out.status.success() {
            let stderr = String::from_utf8_lossy(&create_out.stderr);
            bail!("agent container create failed: {}", stderr.trim());
        }

        let config = self.inspect_container_config(&container_name)?;
        let spec = build_agent_trace_spec(entries, &config);
        let spec_data =
            serde_json::to_vec_pretty(&spec).context("failed to serialize agent trace spec")?;
        fs::write(spec_dir.path().join("spec.json"), spec_data)
            .context("failed to write agent trace spec")?;

        debug!(
            "spawning agent trace container `{}` via {:?}",
            container_name, self.runtime_cmd
        );
        let mut start_cmd = self.base_command();
        start_cmd.arg("start").arg("-a").arg(&container_name);
        let start_status = start_cmd
            .status()
            .context("failed to start agent container")?;
        if !start_status.success() {
            let _ = self.base_command().arg("rm").arg(&container_name).status();
            bail!("agent container `{container_name}` exited with status {start_status}");
        }

        let export_tar = export_dir.path().join("rootfs.tar");
        let tar_file = File::create(&export_tar).context("failed to create agent export tar")?;
        let mut export_cmd = self.base_command();
        export_cmd
            .arg("export")
            .arg(&container_name)
            .stdout(Stdio::from(tar_file));
        let export_status = export_cmd
            .status()
            .context("failed to export agent container")?;
        if !export_status.success() {
            let _ = self.base_command().arg("rm").arg(&container_name).status();
            bail!("agent container export failed for `{container_name}`");
        }

        let unpack_status = Command::new("tar")
            .arg("-C")
            .arg(export_dir.path())
            .arg("-xf")
            .arg(&export_tar)
            .status()
            .context("failed to unpack agent rootfs")?;
        if !unpack_status.success() {
            let _ = self.base_command().arg("rm").arg(&container_name).status();
            bail!("failed to unpack exported rootfs");
        }
        let _ = fs::remove_file(&export_tar);
        let _ = self.base_command().arg("rm").arg(&container_name).status();

        let report_data =
            fs::read(out_dir.path().join("report.json")).context("failed to read agent report")?;
        let report: TraceSpecReport =
            serde_json::from_slice(&report_data).context("failed to parse agent report")?;
        if self.keep_output {
            #[allow(deprecated)]
            let preserved = out_dir.into_path();
            info!(
                "agent trace output for image `{}` preserved at {}",
                reference,
                preserved.display()
            );
        }

        let rootfs = if self.keep_rootfs {
            #[allow(deprecated)]
            let preserved = export_dir.into_path();
            info!(
                "agent rootfs for image `{}` preserved at {}",
                reference,
                preserved.display()
            );
            RootfsExport::preserved(preserved)
        } else {
            RootfsExport::temporary(export_dir)
        };

        Ok(AgentRunResult {
            report,
            rootfs,
            config,
        })
    }

    fn inspect_container_config(&self, container: &str) -> Result<ImageConfig> {
        let mut cmd = self.base_command();
        cmd.arg("inspect")
            .arg(container)
            .arg("--format")
            .arg("{{json .Config}}");
        let output = cmd
            .output()
            .context("failed to inspect agent container config")?;
        if !output.status.success() {
            bail!(
                "agent container inspect failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let cfg: DockerContainerConfig = serde_json::from_slice(&output.stdout)
            .context("failed to decode container config json")?;
        Ok(cfg.into_image_config())
    }

    fn base_command(&self) -> Command {
        let mut command = Command::new(&self.runtime_cmd[0]);
        for arg in &self.runtime_cmd[1..] {
            command.arg(arg);
        }
        command
    }
}

fn parse_engine_command(value: &str) -> Result<Vec<OsString>> {
    let parts = shell_words::split(value)
        .map_err(|err| anyhow::anyhow!("invalid agent CLI command `{value}`: {err}"))?;
    if parts.is_empty() {
        bail!("agent CLI command cannot be empty");
    }
    Ok(parts.into_iter().map(OsString::from).collect())
}

fn bind_mount_arg(source: &Path, target: &str, mode: &str) -> OsString {
    let mut value = OsString::new();
    value.push(source.as_os_str());
    value.push(":");
    value.push(target);
    if !mode.is_empty() {
        value.push(":");
        value.push(mode);
    }
    value
}

fn sanitize_reference(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => ch,
            _ => '-',
        })
        .collect()
}

fn current_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::from_secs(0))
        .as_millis()
}

#[derive(Deserialize)]
struct DockerContainerConfig {
    #[serde(rename = "Env")]
    env: Option<Vec<String>>,
    #[serde(rename = "WorkingDir")]
    working_dir: Option<String>,
    #[serde(rename = "Entrypoint")]
    entrypoint: Option<Vec<String>>,
    #[serde(rename = "Cmd")]
    cmd: Option<Vec<String>>,
}

impl DockerContainerConfig {
    fn into_image_config(self) -> ImageConfig {
        ImageConfig {
            workdir: self
                .working_dir
                .filter(|dir| !dir.is_empty())
                .map(PathBuf::from),
            entrypoint: self.entrypoint.unwrap_or_default(),
            cmd: self.cmd.unwrap_or_default(),
            env: self.env.unwrap_or_default(),
        }
    }
}

pub(crate) struct RootfsExport {
    path: PathBuf,
    cleanup: Option<TempDir>,
}

impl RootfsExport {
    fn temporary(dir: TempDir) -> Self {
        Self {
            path: dir.path().to_path_buf(),
            cleanup: Some(dir),
        }
    }

    fn preserved(path: PathBuf) -> Self {
        Self {
            path,
            cleanup: None,
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn into_parts(self) -> (PathBuf, Option<TempDir>) {
        (self.path, self.cleanup)
    }
}

fn build_agent_trace_spec(entries: &[ImageEntryArg], config: &ImageConfig) -> TraceSpec {
    let mut spec = TraceSpec::new();
    if !config.env.is_empty() {
        spec.env = config
            .env
            .iter()
            .filter_map(|pair| pair.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
    }
    for entry in entries {
        let mut argv = vec![entry.path.display().to_string()];
        if let Some(args) = &entry.trace_args {
            argv.extend(args.clone());
        }
        spec.commands.push(AgentTraceCommand {
            argv,
            cwd: config.workdir.as_ref().map(|p| p.clone()),
        });
    }
    spec
}
