use std::path::{Path, PathBuf};

use log::{debug, info};

use crate::linker::{LinkerError, LinkerRunner};
use sidebundle_core::{BinaryEntryPlan, EntryBundlePlan, ScriptEntryPlan};
use thiserror::Error;

/// Revalidates bundle contents by re-running the linker against packaged entries.
#[derive(Debug, Clone)]
pub struct BundleValidator {
    runner: LinkerRunner,
}

impl BundleValidator {
    pub fn new() -> Self {
        Self {
            runner: LinkerRunner::new(),
        }
    }

    pub fn with_runner(mut self, runner: LinkerRunner) -> Self {
        self.runner = runner;
        self
    }

    /// Validate every entry plan against the files located under `bundle_root`.
    pub fn validate(
        &self,
        bundle_root: &Path,
        plans: &[EntryBundlePlan],
    ) -> Result<(), ValidationError> {
        let report = self.validate_with_report(bundle_root, plans);
        let failure_count = report.failure_count();
        info!(
            "validated {} bundle entr{} ({} failure{})",
            report.entries.len(),
            if report.entries.len() == 1 {
                "y"
            } else {
                "ies"
            },
            failure_count,
            if failure_count == 1 { "" } else { "s" }
        );
        if report.all_passed() {
            Ok(())
        } else {
            Err(ValidationError::Failed { report })
        }
    }

    /// Produce a detailed validation report without short-circuiting on failures.
    pub fn validate_with_report(
        &self,
        bundle_root: &Path,
        plans: &[EntryBundlePlan],
    ) -> ValidationReport {
        let mut entries = Vec::new();
        for plan in plans {
            entries.push(self.inspect_entry(bundle_root, plan));
        }
        ValidationReport { entries }
    }

    fn inspect_entry(&self, bundle_root: &Path, plan: &EntryBundlePlan) -> EntryValidation {
        match plan {
            EntryBundlePlan::Binary(plan) => self.inspect_binary(bundle_root, plan),
            EntryBundlePlan::Script(plan) => self.inspect_script(bundle_root, plan),
        }
    }

    fn inspect_binary(&self, bundle_root: &Path, plan: &BinaryEntryPlan) -> EntryValidation {
        let binary_path = bundle_root.join(&plan.binary_destination);
        let mut validation = EntryValidation {
            display_name: plan.display_name.clone(),
            binary_path: binary_path.clone(),
            status: EntryValidationStatus::StaticOk,
        };

        if !binary_path.exists() {
            validation.status = EntryValidationStatus::MissingBinary;
            return validation;
        }
        if !plan.requires_linker {
            debug!(
                "entry `{}` is static, skipping linker validation",
                plan.display_name
            );
            validation.status = EntryValidationStatus::StaticOk;
            return validation;
        }

        let linker_path = bundle_root.join(&plan.linker_destination);
        if !linker_path.exists() {
            validation.status = EntryValidationStatus::MissingLinker;
            return validation;
        }

        let search_paths: Vec<PathBuf> = plan
            .library_dirs
            .iter()
            .map(|dir| bundle_root.join(dir))
            .collect();

        match self
            .runner
            .trace_dependencies(&linker_path, &binary_path, &search_paths)
        {
            Ok(resolved) => {
                validation.status = EntryValidationStatus::DynamicOk {
                    resolved: resolved.len(),
                };
            }
            Err(err) => {
                if matches!(err, LinkerError::UnsupportedStub { .. }) {
                    validation.status = EntryValidationStatus::LinkerSkipped {
                        reason: err.to_string(),
                    };
                    return validation;
                }
                validation.status = EntryValidationStatus::LinkerError {
                    error: LinkerFailure::from(err),
                };
            }
        }
        validation
    }

    fn inspect_script(&self, bundle_root: &Path, plan: &ScriptEntryPlan) -> EntryValidation {
        let script_path = bundle_root.join(&plan.script_destination);
        let mut validation = EntryValidation {
            display_name: plan.display_name.clone(),
            binary_path: script_path.clone(),
            status: EntryValidationStatus::MissingBinary,
        };

        if !script_path.exists() {
            validation.status = EntryValidationStatus::MissingBinary;
            return validation;
        }
        let interpreter_path = bundle_root.join(&plan.interpreter_destination);
        if !interpreter_path.exists() {
            validation.status = EntryValidationStatus::MissingInterpreter;
            return validation;
        }
        if !plan.requires_linker {
            validation.status = EntryValidationStatus::StaticOk;
            return validation;
        }
        let linker_path = bundle_root.join(&plan.linker_destination);
        if !linker_path.exists() {
            validation.status = EntryValidationStatus::MissingLinker;
            return validation;
        }
        let search_paths: Vec<PathBuf> = plan
            .library_dirs
            .iter()
            .map(|dir| bundle_root.join(dir))
            .collect();

        match self
            .runner
            .trace_dependencies(&linker_path, &interpreter_path, &search_paths)
        {
            Ok(resolved) => {
                validation.status = EntryValidationStatus::DynamicOk {
                    resolved: resolved.len(),
                };
            }
            Err(err) => {
                if matches!(err, LinkerError::UnsupportedStub { .. }) {
                    validation.status = EntryValidationStatus::LinkerSkipped {
                        reason: err.to_string(),
                    };
                    return validation;
                }
                validation.status = EntryValidationStatus::LinkerError {
                    error: LinkerFailure::from(err),
                };
            }
        }
        validation
    }
}

#[derive(Debug, Error)]
pub enum ValidationError {
    #[error("bundle validation failed")]
    Failed { report: ValidationReport },
}

impl ValidationError {
    pub fn report(&self) -> &ValidationReport {
        match self {
            ValidationError::Failed { report } => report,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ValidationReport {
    pub entries: Vec<EntryValidation>,
}

impl ValidationReport {
    pub fn all_passed(&self) -> bool {
        self.entries.iter().all(|entry| entry.status.is_success())
    }

    pub fn failures(&self) -> impl Iterator<Item = &EntryValidation> {
        self.entries
            .iter()
            .filter(|entry| !entry.status.is_success())
    }

    pub fn failure_count(&self) -> usize {
        self.failures().count()
    }
}

#[derive(Debug, Clone)]
pub struct EntryValidation {
    pub display_name: String,
    pub binary_path: PathBuf,
    pub status: EntryValidationStatus,
}

#[derive(Debug, Clone)]
pub enum EntryValidationStatus {
    StaticOk,
    DynamicOk { resolved: usize },
    LinkerSkipped { reason: String },
    MissingBinary,
    MissingLinker,
    MissingInterpreter,
    LinkerError { error: LinkerFailure },
}

impl EntryValidationStatus {
    pub fn is_success(&self) -> bool {
        matches!(
            self,
            EntryValidationStatus::StaticOk
                | EntryValidationStatus::DynamicOk { .. }
                | EntryValidationStatus::LinkerSkipped { .. }
        )
    }
}

#[derive(Debug, Clone)]
pub enum LinkerFailure {
    Spawn {
        linker: PathBuf,
        message: String,
    },
    CommandFailed {
        linker: PathBuf,
        status: Option<i32>,
        stderr: String,
    },
    LibraryNotFound {
        name: String,
        raw: String,
    },
    InvalidPath {
        path: PathBuf,
    },
    Other {
        message: String,
    },
    UnsupportedStub {
        linker: PathBuf,
        message: String,
    },
}

impl From<LinkerError> for LinkerFailure {
    fn from(value: LinkerError) -> Self {
        match value {
            LinkerError::Spawn { linker, source } => LinkerFailure::Spawn {
                linker,
                message: source.to_string(),
            },
            LinkerError::CommandFailed {
                linker,
                status,
                stderr,
            } => LinkerFailure::CommandFailed {
                linker,
                status,
                stderr,
            },
            LinkerError::LibraryNotFound { name, raw } => {
                LinkerFailure::LibraryNotFound { name, raw }
            }
            LinkerError::InvalidPath(path) => LinkerFailure::InvalidPath { path },
            LinkerError::UnsupportedStub { linker, message } => LinkerFailure::UnsupportedStub {
                linker,
                message,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sidebundle_core::{BinaryEntryPlan, EntryBundlePlan, Origin};
    use std::fs;
    use tempfile::tempdir;

    fn dummy_plan(require_linker: bool) -> EntryBundlePlan {
        EntryBundlePlan::Binary(BinaryEntryPlan {
            display_name: "demo".into(),
            binary_source: PathBuf::from("/bin/echo"),
            binary_destination: PathBuf::from("payload/bin/demo"),
            linker_source: PathBuf::from("/lib64/ld-linux-x86-64.so.2"),
            linker_destination: PathBuf::from("payload/lib64/ld-linux-x86-64.so.2"),
            library_dirs: vec![PathBuf::from("payload/lib64")],
            requires_linker: require_linker,
            origin: Origin::Host,
        })
    }

    #[test]
    fn missing_binary_is_reported() {
        let validator = BundleValidator::new();
        let tmp = tempdir().unwrap();
        let report = validator.validate_with_report(tmp.path(), &[dummy_plan(true)]);
        assert!(!report.all_passed());
        assert!(matches!(
            report.entries[0].status,
            EntryValidationStatus::MissingBinary
        ));
    }

    #[test]
    fn static_entry_only_checks_binary() {
        let validator = BundleValidator::new();
        let tmp = tempdir().unwrap();
        let plan = dummy_plan(false);
        let binary_path = tmp.path().join(
            match &plan {
                EntryBundlePlan::Binary(inner) => &inner.binary_destination,
                _ => unreachable!(),
            },
        );
        fs::create_dir_all(binary_path.parent().unwrap()).unwrap();
        fs::write(&binary_path, b"#!/bin/true\n").unwrap();
        let report = validator.validate_with_report(tmp.path(), &[plan]);
        assert!(report.all_passed());
        assert!(matches!(
            report.entries[0].status,
            EntryValidationStatus::StaticOk
        ));
    }
}
