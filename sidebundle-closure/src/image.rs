use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[allow(deprecated)]
use bollard::container::{Config as ContainerConfig, CreateContainerOptions, RemoveContainerOptions};
use bollard::errors::Error as BollardError;
use bollard::{Docker, API_DEFAULT_VERSION};
use futures_util::StreamExt;
use log::warn;
use serde_json::Value;
use tar::Archive;
use tempfile::TempDir;
use thiserror::Error;
use tokio::fs::File as TokioFile;
use tokio::io::AsyncWriteExt;
use tokio::runtime::Builder as RuntimeBuilder;
use tokio::task;

/// Metadata extracted from an OCI/Container image config that may impact runtime.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageConfig {
    pub workdir: Option<PathBuf>,
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
    pub env: Vec<String>,
}

impl ImageConfig {
    pub fn is_empty(&self) -> bool {
        self.workdir.is_none() && self.entrypoint.is_empty() && self.cmd.is_empty() && self.env.is_empty()
    }
}

/// Handle representing a prepared image rootfs and associated metadata.
pub struct ImageRoot {
    reference: String,
    rootfs_path: PathBuf,
    config: ImageConfig,
    cleanup: Option<Box<dyn CleanupHook>>,
}

impl ImageRoot {
    pub fn new(reference: impl Into<String>, rootfs_path: impl Into<PathBuf>, config: ImageConfig) -> Self {
        Self {
            reference: reference.into(),
            rootfs_path: rootfs_path.into(),
            config,
            cleanup: None,
        }
    }

    pub fn reference(&self) -> &str {
        &self.reference
    }

    pub fn rootfs(&self) -> &Path {
        &self.rootfs_path
    }

    pub fn config(&self) -> &ImageConfig {
        &self.config
    }

    pub fn with_cleanup<F>(mut self, cleanup: F) -> Self
    where
        F: FnOnce() + Send + 'static,
    {
        self.cleanup = Some(Box::new(cleanup));
        self
    }

    pub fn detach_cleanup(mut self) -> Self {
        self.cleanup = None;
        self
    }

    pub fn into_parts(mut self) -> (String, PathBuf, ImageConfig) {
        self.cleanup = None;
        let reference = std::mem::take(&mut self.reference);
        let rootfs = std::mem::take(&mut self.rootfs_path);
        let config = std::mem::take(&mut self.config);
        (reference, rootfs, config)
    }
}

impl Drop for ImageRoot {
    fn drop(&mut self) {
        if let Some(cleanup) = self.cleanup.take() {
            cleanup.call();
        }
    }
}

trait CleanupHook: Send {
    fn call(self: Box<Self>);
}

impl<F> CleanupHook for F
where
    F: FnOnce(),
    F: Send + 'static,
{
    fn call(self: Box<Self>) {
        (*self)();
    }
}

/// Interface for backends that can materialize an image's root filesystem locally.
pub trait ImageRootProvider: Send + Sync {
    fn backend(&self) -> &'static str;

    fn prepare_root(&self, reference: &str) -> Result<ImageRoot, ImageProviderError>;
}

/// Docker implementation of [`ImageRootProvider`], favoring Bollard (native API) with CLI fallback.
#[derive(Debug, Clone)]
pub struct DockerProvider {
    cli_path: PathBuf,
}

impl Default for DockerProvider {
    fn default() -> Self {
        Self {
            cli_path: PathBuf::from("docker"),
        }
    }
}

impl DockerProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_cli_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.cli_path = path.into();
        self
    }

    fn create_runtime(&self) -> Result<tokio::runtime::Runtime, ImageProviderError> {
        RuntimeBuilder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|err| ImageProviderError::Other(format!("failed to init tokio runtime: {err}")))
    }

    async fn prepare_with_bollard(&self, reference: &str) -> Result<ImageRoot, ImageProviderError> {
        let docker = Docker::connect_with_local_defaults().map_err(|err| {
            ImageProviderError::unavailable("docker-bollard", err.to_string())
        })?;
        let docker = docker
            .negotiate_version()
            .await
            .map_err(|err| ImageProviderError::unavailable("docker-bollard", err.to_string()))?;
        prepare_reference_with_bollard(&docker, reference).await
    }

    fn prepare_with_cli(&self, reference: &str) -> Result<ImageRoot, ImageProviderError> {
        let output = run_cli_capture(&self.cli_path, &["create", reference], "docker-cli")?;
        let container_id = output.trim().to_string();
        let guard = CliContainerGuard::new(self, container_id.clone());

        let tempdir = tempfile::tempdir()?;
        let rootfs_path = tempdir.path().to_path_buf();
        let tar_path = tempdir.path().join("rootfs.tar");

        cli_export(&self.cli_path, &container_id, &tar_path, "docker")?;
        unpack_tar_file(&tar_path, &rootfs_path)?;
        let _ = fs::remove_file(&tar_path);

        let config = inspect_image_from_cli(&self.cli_path, reference, "docker-cli")?;
        drop(guard);

        let cleanup_dir = tempdir;
        Ok(ImageRoot::new(reference, rootfs_path, config).with_cleanup(move || drop(cleanup_dir)))
    }

}

impl ImageRootProvider for DockerProvider {
    fn backend(&self) -> &'static str {
        "docker"
    }

    fn prepare_root(&self, reference: &str) -> Result<ImageRoot, ImageProviderError> {
        let trimmed = reference.trim();
        if trimmed.is_empty() {
            return Err(ImageProviderError::EmptyReference);
        }
        let runtime = self.create_runtime()?;
        match runtime.block_on(self.prepare_with_bollard(trimmed)) {
            Ok(root) => Ok(root),
            Err(err) => {
                warn!(
                    "docker bollard path failed ({}), falling back to CLI",
                    err
                );
                self.prepare_with_cli(trimmed)
            }
        }
    }
}

/// Podman implementation prioritizing native CLI mount/export and falling back to Bollard.
#[derive(Debug, Clone)]
pub struct PodmanProvider {
    cli_path: PathBuf,
    service_socket: Option<PathBuf>,
}

impl Default for PodmanProvider {
    fn default() -> Self {
        Self {
            cli_path: PathBuf::from("podman"),
            service_socket: None,
        }
    }
}

impl PodmanProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_cli_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.cli_path = path.into();
        self
    }

    pub fn with_service_socket(mut self, path: impl Into<PathBuf>) -> Self {
        self.service_socket = Some(path.into());
        self
    }

    fn create_runtime(&self) -> Result<tokio::runtime::Runtime, ImageProviderError> {
        RuntimeBuilder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|err| ImageProviderError::Other(format!("failed to init tokio runtime: {err}")))
    }

    fn prepare_with_cli(&self, reference: &str) -> Result<ImageRoot, ImageProviderError> {
        let output = run_cli_capture(&self.cli_path, &["create", reference], "podman-cli")?;
        let container_id = output.trim().to_string();
        if container_id.is_empty() {
            return Err(ImageProviderError::Other(
                "podman create returned empty container id".into(),
            ));
        }
        let mut guard = PodmanContainerGuard::new(self.cli_path.clone(), container_id.clone());

        match self.prepare_cli_mount(reference, &container_id, &mut guard) {
            Ok(root) => return Ok(root),
            Err(err) => {
                warn!("podman mount path unavailable ({err}), falling back to export");
            }
        }

        self.prepare_cli_export(reference, &container_id, &mut guard)
    }

    fn prepare_cli_mount(
        &self,
        reference: &str,
        container_id: &str,
        guard: &mut PodmanContainerGuard,
    ) -> Result<ImageRoot, ImageProviderError> {
        let mount_raw = run_cli_capture(&self.cli_path, &["mount", container_id], "podman-cli")?;
        let mount_path = parse_mount_output(&mount_raw).ok_or_else(|| {
            ImageProviderError::Other("podman mount returned empty output".into())
        })?;

        let config = inspect_image_from_cli(&self.cli_path, reference, "podman-cli")?;
        let cleanup = guard.into_mount_cleanup();
        Ok(ImageRoot::new(reference, mount_path, config).with_cleanup(move || cleanup.cleanup()))
    }

    fn prepare_cli_export(
        &self,
        reference: &str,
        container_id: &str,
        guard: &mut PodmanContainerGuard,
    ) -> Result<ImageRoot, ImageProviderError> {
        let tempdir = tempfile::tempdir()?;
        let rootfs_path = tempdir.path().to_path_buf();
        let tar_path = tempdir.path().join("rootfs.tar");

        cli_export(&self.cli_path, container_id, &tar_path, "podman")?;
        unpack_tar_file(&tar_path, &rootfs_path)?;
        let _ = fs::remove_file(&tar_path);

        let config = inspect_image_from_cli(&self.cli_path, reference, "podman-cli")?;
        guard.remove_now();
        let cleanup_dir = tempdir;
        Ok(ImageRoot::new(reference, rootfs_path, config).with_cleanup(move || drop(cleanup_dir)))
    }

    fn prepare_with_service(&self, reference: &str) -> Result<ImageRoot, ImageProviderError> {
        let (socket_uri, mut guard) = self.start_service()?;
        let runtime = self.create_runtime()?;
        let result = runtime.block_on(async {
            let docker = Docker::connect_with_unix(&socket_uri, 120, &API_DEFAULT_VERSION)
                .map_err(|err| ImageProviderError::unavailable("podman-service", err.to_string()))?;
            let docker = docker
                .negotiate_version()
                .await
                .map_err(|err| ImageProviderError::unavailable("podman-service", err.to_string()))?;
            prepare_reference_with_bollard(&docker, reference).await
        });
        guard.shutdown();
        result
    }

    fn start_service(&self) -> Result<(String, PodmanServiceGuard), ImageProviderError> {
        let (socket_path, tempdir) = if let Some(path) = &self.service_socket {
            (path.clone(), None)
        } else {
            let dir = tempfile::tempdir()?;
            (dir.path().join("podman.sock"), Some(dir))
        };
        if let Some(parent) = socket_path.parent() {
            fs::create_dir_all(parent)?;
        }
        if socket_path.exists() {
            let _ = fs::remove_file(&socket_path);
        }
        let socket_uri = format!("unix://{}", socket_path.display());
        let mut child = Command::new(&self.cli_path)
            .args(["system", "service", "--time=0", &socket_uri])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|err| ImageProviderError::unavailable("podman-service", err.to_string()))?;
        match wait_for_socket(&socket_path, Duration::from_secs(5)) {
            Ok(()) => Ok((socket_uri, PodmanServiceGuard::new(child, tempdir))),
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(err)
            }
        }
    }
}

impl ImageRootProvider for PodmanProvider {
    fn backend(&self) -> &'static str {
        "podman"
    }

    fn prepare_root(&self, reference: &str) -> Result<ImageRoot, ImageProviderError> {
        let trimmed = reference.trim();
        if trimmed.is_empty() {
            return Err(ImageProviderError::EmptyReference);
        }
        match self.prepare_with_cli(trimmed) {
            Ok(root) => Ok(root),
            Err(err) => {
                warn!("podman CLI path failed ({err}), falling back to system service");
                self.prepare_with_service(trimmed)
            }
        }
    }
}

struct CliContainerGuard<'a> {
    provider: &'a DockerProvider,
    id: String,
}

impl<'a> CliContainerGuard<'a> {
    fn new(provider: &'a DockerProvider, id: String) -> Self {
        Self { provider, id }
    }
}

impl<'a> Drop for CliContainerGuard<'a> {
    fn drop(&mut self) {
        let _ = Command::new(&self.provider.cli_path)
            .args(["rm", "-f", &self.id])
            .status();
    }
}

struct PodmanContainerGuard {
    cli_path: PathBuf,
    id: String,
    active: bool,
}

impl PodmanContainerGuard {
    fn new(cli_path: PathBuf, id: String) -> Self {
        Self {
            cli_path,
            id,
            active: true,
        }
    }

    fn remove_now(&mut self) {
        if self.active {
            let _ = Command::new(&self.cli_path)
                .args(["rm", "-f", &self.id])
                .status();
            self.active = false;
        }
    }

    fn into_mount_cleanup(&mut self) -> PodmanMountCleanup {
        self.active = false;
        PodmanMountCleanup {
            cli_path: self.cli_path.clone(),
            container_id: self.id.clone(),
        }
    }
}

impl Drop for PodmanContainerGuard {
    fn drop(&mut self) {
        self.remove_now();
    }
}

struct PodmanMountCleanup {
    cli_path: PathBuf,
    container_id: String,
}

impl PodmanMountCleanup {
    fn cleanup(self) {
        let _ = Command::new(&self.cli_path)
            .args(["unmount", &self.container_id])
            .status();
        let _ = Command::new(&self.cli_path)
            .args(["rm", "-f", &self.container_id])
            .status();
    }
}

struct PodmanServiceGuard {
    child: Option<Child>,
    tempdir: Option<TempDir>,
}

impl PodmanServiceGuard {
    fn new(child: Child, tempdir: Option<TempDir>) -> Self {
        Self {
            child: Some(child),
            tempdir,
        }
    }

    fn shutdown(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.tempdir.take();
    }
}

impl Drop for PodmanServiceGuard {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Debug, Error)]
pub enum ImageProviderError {
    #[error("provider `{backend}` unavailable: {reason}")]
    Unavailable { backend: &'static str, reason: String },
    #[error("image reference is empty")]
    EmptyReference,
    #[error("image `{reference}` not found: {message}")]
    NotFound { reference: String, message: String },
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

impl ImageProviderError {
    pub fn unavailable(backend: &'static str, reason: impl Into<String>) -> Self {
        Self::Unavailable {
            backend,
            reason: reason.into(),
        }
    }

    pub fn not_found(reference: impl Into<String>, message: impl Into<String>) -> Self {
        Self::NotFound {
            reference: reference.into(),
            message: message.into(),
        }
    }
}

impl fmt::Debug for ImageRoot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImageRoot")
            .field("reference", &self.reference)
            .field("rootfs_path", &self.rootfs_path)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

fn unpack_tar_file(tar_path: &Path, dest: &Path) -> Result<(), ImageProviderError> {
    let file = fs::File::open(tar_path)?;
    let mut archive = Archive::new(file);
    archive.unpack(dest)?;
    Ok(())
}

fn image_config_from_value(value: &Value) -> ImageConfig {
    let config = value.get("Config").and_then(|cfg| cfg.as_object());
    let mut result = ImageConfig::default();
    if let Some(cfg) = config {
        if let Some(workdir) = cfg
            .get("WorkingDir")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            result.workdir = Some(PathBuf::from(workdir));
        }
        if let Some(entrypoint) = cfg.get("Entrypoint").and_then(|v| v.as_array()) {
            result.entrypoint = entrypoint
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
        }
        if let Some(cmd) = cfg.get("Cmd").and_then(|v| v.as_array()) {
            result.cmd = cmd
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
        }
        if let Some(env) = cfg.get("Env").and_then(|v| v.as_array()) {
            result.env = env
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
        }
    }
    result
}

fn map_bollard_error(action: &str, err: BollardError) -> ImageProviderError {
    match err {
        BollardError::DockerResponseServerError { message, .. } => {
            ImageProviderError::Other(format!("{action} failed: {message}"))
        }
        BollardError::IOError { err } => ImageProviderError::Io(err),
        other => ImageProviderError::Other(format!("{action} failed: {other}")),
    }
}

fn run_cli_capture(
    cli_path: &Path,
    args: &[&str],
    backend: &'static str,
) -> Result<String, ImageProviderError> {
    let output = Command::new(cli_path)
        .args(args)
        .output()
        .map_err(|err| ImageProviderError::unavailable(backend, err.to_string()))?;
    if !output.status.success() {
        return Err(ImageProviderError::Other(format!(
            "`{} {}` failed: {}",
            cli_path.display(),
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn cli_export(
    cli_path: &Path,
    container_id: &str,
    tar_path: &Path,
    backend: &'static str,
) -> Result<(), ImageProviderError> {
    let mut child = Command::new(cli_path)
        .arg("export")
        .arg(container_id)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|err| ImageProviderError::unavailable(backend, err.to_string()))?;
    let mut reader = child
        .stdout
        .take()
        .ok_or_else(|| ImageProviderError::Other("export produced no stdout".into()))?;
    let mut file = fs::File::create(tar_path)?;
    io::copy(&mut reader, &mut file)?;
    let status = child.wait()?;
    if !status.success() {
        return Err(ImageProviderError::Other(format!(
            "`{} export` exited with {status}",
            cli_path.display()
        )));
    }
    Ok(())
}

fn inspect_image_from_cli(
    cli_path: &Path,
    reference: &str,
    backend: &'static str,
) -> Result<ImageConfig, ImageProviderError> {
    let raw = run_cli_capture(cli_path, &["image", "inspect", reference], backend)?;
    parse_inspect_output(&raw)
}

fn parse_inspect_output(raw: &str) -> Result<ImageConfig, ImageProviderError> {
    let inspect_json: Value = serde_json::from_str(raw)
        .map_err(|err| ImageProviderError::Other(format!("inspect JSON parse error: {err}")))?;
    let config_value = inspect_json
        .as_array()
        .and_then(|arr| arr.first())
        .cloned()
        .unwrap_or(Value::Null);
    Ok(image_config_from_value(&config_value))
}

#[allow(deprecated)]
async fn prepare_reference_with_bollard(
    docker: &Docker,
    reference: &str,
) -> Result<ImageRoot, ImageProviderError> {
    let create = docker
        .create_container(
            Some(CreateContainerOptions { name: "", platform: None }),
            ContainerConfig {
                image: Some(reference),
                ..Default::default()
            },
        )
        .await
        .map_err(|err| map_bollard_error("create_container", err))?;

    let container_id = create.id;
    let result = export_root_with_bollard(docker, &container_id, reference).await;

    let _ = docker
        .remove_container(
            &container_id,
            Some(RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await;
    result
}

async fn export_root_with_bollard(
    docker: &Docker,
    container_id: &str,
    reference: &str,
) -> Result<ImageRoot, ImageProviderError> {
    let inspect = docker
        .inspect_image(reference)
        .await
        .map_err(|err| map_bollard_error("inspect_image", err))?;
    let inspect_value =
        serde_json::to_value(&inspect).map_err(|err| ImageProviderError::Other(err.to_string()))?;
    let config = image_config_from_value(&inspect_value);

    let tempdir = tempfile::tempdir()?;
    let rootfs_path = tempdir.path().to_path_buf();
    let tar_path = tempdir.path().join("rootfs.tar");

    let mut stream = docker.export_container(container_id);
    let mut file = TokioFile::create(&tar_path).await?;
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(|err| map_bollard_error("export_container", err))?;
        file.write_all(bytes.as_ref()).await?;
    }
    file.sync_all().await?;
    drop(file);

    let tar_clone = tar_path.clone();
    let rootfs_clone = rootfs_path.clone();
    task::spawn_blocking(move || {
        unpack_tar_file(&tar_clone, &rootfs_clone)?;
        let _ = fs::remove_file(&tar_clone);
        Ok::<(), ImageProviderError>(())
    })
    .await
    .map_err(|err| ImageProviderError::Other(format!("unpack task failed: {err}")))??;

    let cleanup_dir = tempdir;
    Ok(ImageRoot::new(reference, rootfs_path, config).with_cleanup(move || drop(cleanup_dir)))
}

fn parse_mount_output(raw: &str) -> Option<PathBuf> {
    raw.lines()
        .map(|line| line.trim())
        .find(|line| !line.is_empty())
        .map(PathBuf::from)
}

fn wait_for_socket(path: &Path, timeout: Duration) -> Result<(), ImageProviderError> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(ImageProviderError::Other(format!(
        "podman service socket `{}` not ready",
        path.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    #[test]
    fn config_default_is_empty() {
        let cfg = ImageConfig::default();
        assert!(cfg.is_empty());
    }

    #[test]
    fn cleanup_runs_on_drop() {
        let triggered = Arc::new(AtomicBool::new(false));
        {
            let flag = triggered.clone();
            let config = ImageConfig::default();
            let _root = ImageRoot::new("demo", "/tmp/root", config).with_cleanup(move || {
                flag.store(true, Ordering::SeqCst);
            });
        }
        assert!(triggered.load(Ordering::SeqCst));
    }

    #[test]
    fn into_parts_detaches_cleanup() {
        let triggered = Arc::new(AtomicBool::new(false));
        let flag = triggered.clone();
        let config = ImageConfig::default();
        let root = ImageRoot::new("demo", "/tmp/root", config).with_cleanup(move || {
            flag.store(true, Ordering::SeqCst);
        });
        let (_reference, _path, _config) = root.into_parts();
        assert!(!triggered.load(Ordering::SeqCst));
    }

    #[test]
    fn image_config_parsing() {
        let value = json!({
            "Config": {
                "WorkingDir": "/app",
                "Entrypoint": ["/bin/sh", "-c"],
                "Cmd": ["run", "service"],
                "Env": ["A=1", "B=2"]
            }
        });
        let cfg = image_config_from_value(&value);
        assert_eq!(cfg.workdir, Some(PathBuf::from("/app")));
        assert_eq!(
            cfg.entrypoint,
            vec![String::from("/bin/sh"), String::from("-c")]
        );
        assert_eq!(
            cfg.cmd,
            vec![String::from("run"), String::from("service")]
        );
        assert_eq!(
            cfg.env,
            vec![String::from("A=1"), String::from("B=2")]
        );
    }

    #[test]
    fn mount_output_parsing() {
        let parsed = parse_mount_output(" /tmp/merged \n/tmp/other");
        assert_eq!(parsed, Some(PathBuf::from("/tmp/merged")));
        assert!(parse_mount_output("   \n  ").is_none());
    }
}
