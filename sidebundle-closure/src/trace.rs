use crate::PathResolver;
use nix::errno::Errno;
use sidebundle_core::LogicalPath;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

mod agent;

pub use agent::{
    AgentEngine, AgentEngineError, AgentTraceBackend, TraceCommand as AgentTraceCommand,
    TraceLimits, TraceSpec, TraceSpecReport, TRACE_REPORT_VERSION, TRACE_SPEC_VERSION,
};

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
pub use linux::{CombinedBackend, FanotifyBackend, PtraceBackend};

/// Trace runner wrapper that allows swapping implementations.
#[derive(Debug, Clone)]
pub struct TraceCollector {
    backend: TraceBackendKind,
    env: Vec<(OsString, OsString)>,
}

impl TraceCollector {
    pub fn new() -> Self {
        Self {
            backend: TraceBackendKind::default(),
            env: Vec::new(),
        }
    }

    pub fn with_env(mut self, env: Vec<(OsString, OsString)>) -> Self {
        self.env = env;
        self
    }

    pub fn with_backend(mut self, backend: TraceBackendKind) -> Self {
        self.backend = backend;
        self
    }

    pub fn run(
        &self,
        resolver: &dyn PathResolver,
        command: &TraceCommand,
    ) -> Result<Vec<TraceArtifact>, TraceError> {
        let mut argv = Vec::with_capacity(1 + command.args().len());
        let program = resolver.to_trace_path(command.program());
        argv.push(program.display().to_string());
        argv.extend(command.args().iter().cloned());
        let invocation = TraceInvocation {
            command: &argv,
            root: resolver.trace_root(),
            env: &self.env,
        };
        let report = self.backend.trace(&invocation)?;
        Ok(report.into_artifacts(resolver))
    }
}

/// Logical trace command executed by the backend.
#[derive(Debug, Clone)]
pub struct TraceCommand {
    program: LogicalPath,
    args: Vec<String>,
}

impl TraceCommand {
    pub fn new(program: LogicalPath) -> Self {
        Self {
            program,
            args: Vec::new(),
        }
    }

    pub fn with_args(mut self, args: Vec<String>) -> Self {
        self.args = args;
        self
    }

    pub fn program(&self) -> &LogicalPath {
        &self.program
    }

    pub fn args(&self) -> &[String] {
        &self.args
    }
}

/// Execution context passed to trace backends.
pub struct TraceInvocation<'a> {
    pub command: &'a [String],
    pub root: Option<&'a Path>,
    pub env: &'a [(OsString, OsString)],
}

/// Common trait implemented by concrete tracing backends.
pub trait TraceBackend {
    fn trace(&self, invocation: &TraceInvocation<'_>) -> Result<TraceReport, TraceError>;
}

/// Concrete backend selector used by TraceCollector.
#[derive(Debug, Clone)]
pub enum TraceBackendKind {
    Null(NullBackend),
    #[cfg(target_os = "linux")]
    Ptrace(PtraceBackend),
    #[cfg(target_os = "linux")]
    Fanotify(FanotifyBackend),
    #[cfg(target_os = "linux")]
    Combined(CombinedBackend),
    Agent(AgentTraceBackend),
}

impl TraceBackendKind {
    pub fn null() -> Self {
        Self::Null(NullBackend::new())
    }

    #[cfg(target_os = "linux")]
    pub fn ptrace() -> Self {
        Self::Ptrace(PtraceBackend::new())
    }

    #[cfg(target_os = "linux")]
    pub fn fanotify() -> Self {
        Self::Fanotify(FanotifyBackend::new())
    }

    #[cfg(target_os = "linux")]
    pub fn combined() -> Self {
        Self::Combined(CombinedBackend::new())
    }

    fn trace(&self, invocation: &TraceInvocation<'_>) -> Result<TraceReport, TraceError> {
        match self {
            TraceBackendKind::Null(backend) => backend.trace(invocation),
            #[cfg(target_os = "linux")]
            TraceBackendKind::Ptrace(backend) => backend.trace(invocation),
            #[cfg(target_os = "linux")]
            TraceBackendKind::Fanotify(backend) => backend.trace(invocation),
            #[cfg(target_os = "linux")]
            TraceBackendKind::Combined(backend) => backend.trace(invocation),
            TraceBackendKind::Agent(backend) => backend.trace(invocation),
        }
    }
}

impl Default for TraceBackendKind {
    fn default() -> Self {
        #[cfg(target_os = "linux")]
        {
            return TraceBackendKind::ptrace();
        }
        #[cfg(not(target_os = "linux"))]
        {
            TraceBackendKind::Null(NullBackend::unsupported())
        }
    }
}

/// Trace backend that performs no collection.
#[derive(Debug, Clone, Default)]
pub struct NullBackend {
    fail_on_use: bool,
}

impl NullBackend {
    pub fn new() -> Self {
        Self { fail_on_use: false }
    }

    pub fn unsupported() -> Self {
        Self { fail_on_use: true }
    }
}

impl TraceBackend for NullBackend {
    fn trace(&self, _invocation: &TraceInvocation<'_>) -> Result<TraceReport, TraceError> {
        if self.fail_on_use {
            Err(TraceError::Unsupported(
                "runtime tracing is only available on Linux",
            ))
        } else {
            Ok(TraceReport::default())
        }
    }
}

/// Aggregated trace output.
#[derive(Debug, Clone)]
pub struct TraceArtifact {
    pub runtime_path: PathBuf,
    pub host_path: Option<PathBuf>,
    pub logical_path: Option<LogicalPath>,
}

#[derive(Debug, Clone, Default)]
pub struct TraceReport {
    pub files: BTreeSet<PathBuf>,
}

impl TraceReport {
    pub fn record_path(&mut self, path: PathBuf) {
        if path.as_os_str().is_empty() {
            return;
        }
        self.files.insert(path);
    }

    pub fn extend(&mut self, other: TraceReport) {
        for path in other.files {
            self.files.insert(path);
        }
    }

    pub fn into_artifacts(self, resolver: &dyn PathResolver) -> Vec<TraceArtifact> {
        self.files
            .into_iter()
            .map(|runtime_path| {
                let host_path = resolver.runtime_to_host(&runtime_path);
                let logical_path = host_path
                    .as_ref()
                    .and_then(|path| resolver.host_to_logical(path));
                TraceArtifact {
                    runtime_path,
                    host_path,
                    logical_path,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_dedup() {
        let mut report = TraceReport::default();
        report.record_path(PathBuf::from("/tmp/a"));
        report.record_path(PathBuf::from("/tmp/a"));
        assert_eq!(report.files.len(), 1);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TraceError {
    #[error("empty command")]
    EmptyCommand,
    #[error("failed to convert string: {0}")]
    CString(#[from] std::ffi::NulError),
    #[error("IO: {0}")]
    Io(#[from] std::io::Error),
    #[error("nix error: {0}")]
    Nix(Errno),
    #[error("ptrace not permitted: {0}")]
    Permission(String),
    #[error("traced process exited unexpectedly")]
    UnexpectedExit,
    #[error("fanotify unavailable: {0}")]
    Fanotify(String),
    #[error("{0}")]
    Unsupported(&'static str),
    #[error("agent backend error: {0}")]
    Agent(String),
}

#[cfg(not(target_os = "linux"))]
mod linux {}
