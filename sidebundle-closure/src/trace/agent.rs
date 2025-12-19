use crate::trace::{TraceBackend, TraceError, TraceInvocation, TraceReport};
use serde::{Deserialize, Serialize};
use sidebundle_core::{RuntimeMetadata, TraceAccess};
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

/// Current schema version for trace specs exchanged with container agents.
pub const TRACE_SPEC_VERSION: u32 = 1;
/// Current schema version for reports emitted by container agents.
pub const TRACE_REPORT_VERSION: u32 = 2;

/// Serializable specification describing commands the agent must trace.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TraceSpec {
    pub schema_version: u32,
    #[serde(default)]
    pub commands: Vec<TraceCommand>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub limits: TraceLimits,
}

impl TraceSpec {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_command(mut self, command: TraceCommand) -> Self {
        self.commands.push(command);
        self
    }
}

/// Single command that should be traced by the agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceCommand {
    pub argv: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
}

/// Limits applied by the agent (timeouts, event caps, etc.).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TraceLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_events: Option<u64>,
}

/// Serialized report emitted by the agent after tracing finishes.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TraceSpecReport {
    pub schema_version: u32,
    #[serde(default)]
    pub files: Vec<TraceSpecRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<RuntimeMetadata>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceSpecRecord {
    pub path: PathBuf,
    pub access: TraceAccess,
}

impl TraceSpecReport {
    pub fn into_runtime_report(self) -> TraceReport {
        let mut report = TraceReport::default();
        for record in self.files {
            report.record_path_with_access(record.path, record.access);
        }
        report
    }
}

/// Trait implemented by executors capable of running the agent.
pub trait AgentEngine: Send + Sync {
    fn run(&self, spec: &TraceSpec) -> Result<TraceSpecReport, AgentEngineError>;
}

/// Default engine that simply reports the backend as unavailable.
#[derive(Debug, Clone, Default)]
pub struct NullAgentEngine;

impl AgentEngine for NullAgentEngine {
    fn run(&self, _spec: &TraceSpec) -> Result<TraceSpecReport, AgentEngineError> {
        Err(AgentEngineError::Unsupported(
            "agent backend is not configured",
        ))
    }
}

/// Error returned by agent engines.
#[derive(Debug, thiserror::Error)]
pub enum AgentEngineError {
    #[error("agent backend unsupported: {0}")]
    Unsupported(&'static str),
    #[error("agent backend failed: {0}")]
    Failure(String),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<AgentEngineError> for TraceError {
    fn from(err: AgentEngineError) -> Self {
        match err {
            AgentEngineError::Unsupported(message) => {
                TraceError::Agent(format!("agent backend unsupported: {message}"))
            }
            AgentEngineError::Failure(message) => TraceError::Agent(message),
            AgentEngineError::Serialization(error) => {
                TraceError::Agent(format!("failed to serialize trace spec: {error}"))
            }
            AgentEngineError::Io(error) => TraceError::Agent(format!("IO error: {error}")),
        }
    }
}

/// Trace backend implementation that delegates to an external agent.
#[derive(Clone)]
pub struct AgentTraceBackend {
    engine: Arc<dyn AgentEngine>,
    limits: TraceLimits,
}

impl AgentTraceBackend {
    pub fn new() -> Self {
        Self {
            engine: Arc::new(NullAgentEngine),
            limits: TraceLimits::default(),
        }
    }

    pub fn with_engine<E>(mut self, engine: E) -> Self
    where
        E: AgentEngine + 'static,
    {
        self.engine = Arc::new(engine);
        self
    }

    pub fn with_limits(mut self, limits: TraceLimits) -> Self {
        self.limits = limits;
        self
    }
}

impl Default for AgentTraceBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl TraceBackend for AgentTraceBackend {
    fn trace(&self, invocation: &TraceInvocation<'_>) -> Result<TraceReport, TraceError> {
        if invocation.command.is_empty() {
            return Err(TraceError::EmptyCommand);
        }
        let mut spec = TraceSpec::new();
        spec.limits = self.limits.clone();
        spec.commands.push(TraceCommand {
            argv: invocation.command.to_vec(),
            cwd: None,
        });
        if !invocation.env.is_empty() {
            let env = invocation
                .env
                .iter()
                .map(|(key, value)| {
                    (
                        key.to_string_lossy().into_owned(),
                        value.to_string_lossy().into_owned(),
                    )
                })
                .collect();
            spec.env = env;
        }
        let report = self.engine.run(&spec)?;
        Ok(report.into_runtime_report())
    }
}

impl fmt::Debug for AgentTraceBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentTraceBackend").finish()
    }
}
