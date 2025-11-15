mod elf;

pub use elf::{parse_elf_metadata, ElfMetadata, ElfParseError};

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt::{self, Display};
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// 架构枚举，目前仅支持 x86_64，但预留扩展。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetArch {
    X86_64,
}

/// 操作系统枚举，MVP 主攻 Linux。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetOs {
    Linux,
}

/// 目标三元组，用于后续扩展到多平台。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetTriple {
    pub arch: TargetArch,
    pub os: TargetOs,
}

impl TargetTriple {
    pub const fn linux_x86_64() -> Self {
        Self {
            arch: TargetArch::X86_64,
            os: TargetOs::Linux,
        }
    }

    pub const fn as_str(&self) -> &'static str {
        match (self.os, self.arch) {
            (TargetOs::Linux, TargetArch::X86_64) => "linux-x86_64",
        }
    }

    pub fn parse(value: &str) -> Result<Self, TargetParseError> {
        match value {
            "linux-x86_64" => Ok(TargetTriple::linux_x86_64()),
            other => Err(TargetParseError {
                provided: other.to_string(),
            }),
        }
    }
}

impl Display for TargetTriple {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct TargetParseError {
    provided: String,
}

impl Display for TargetParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unsupported target triple: {}", self.provided)
    }
}

impl Error for TargetParseError {}

impl FromStr for TargetTriple {
    type Err = TargetParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        TargetTriple::parse(value)
    }
}

/// 用户声明的入口信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicalPath {
    origin: Origin,
    path: PathBuf,
}

impl LogicalPath {
    pub fn new(origin: Origin, path: impl Into<PathBuf>) -> Self {
        Self {
            origin,
            path: path.into(),
        }
    }

    pub fn origin(&self) -> &Origin {
        &self.origin
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Origin {
    Host,
    Image(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleEntry {
    pub logical: LogicalPath,
    pub display_name: String,
    pub trace_args: Option<Vec<String>>,
}

impl BundleEntry {
    pub fn new(logical: LogicalPath, display_name: impl Into<String>) -> Self {
        Self {
            logical,
            display_name: display_name.into(),
            trace_args: None,
        }
    }

    pub fn with_trace_args(mut self, args: Vec<String>) -> Self {
        self.trace_args = Some(args);
        self
    }
}

/// Manifest/CLI 汇总后的 bundle 规格。
#[derive(Debug, Clone)]
pub struct BundleSpec {
    pub name: String,
    pub target: TargetTriple,
    pub entries: Vec<BundleEntry>,
}

impl BundleSpec {
    pub fn new(name: impl Into<String>, target: TargetTriple) -> Self {
        Self {
            name: name.into(),
            target,
            entries: Vec::new(),
        }
    }

    pub fn with_entry(mut self, entry: BundleEntry) -> Self {
        self.entries.push(entry);
        self
    }

    pub fn push_entry(&mut self, entry: BundleEntry) {
        self.entries.push(entry);
    }

    pub fn host_entry(path: impl Into<PathBuf>, display_name: impl Into<String>) -> BundleEntry {
        BundleEntry::new(LogicalPath::new(Origin::Host, path), display_name)
    }

    pub fn entries(&self) -> &[BundleEntry] {
        &self.entries
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn target(&self) -> TargetTriple {
        self.target
    }
}

/// 依赖闭包中的单个文件映射。
#[derive(Debug, Clone)]
pub struct ResolvedFile {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub digest: String,
}

impl ResolvedFile {
    pub fn new(
        source: impl Into<PathBuf>,
        destination: impl Into<PathBuf>,
        digest: impl Into<String>,
    ) -> Self {
        Self {
            source: source.into(),
            destination: destination.into(),
            digest: digest.into(),
        }
    }
}

/// 针对单个入口生成 launcher 所需的信息。
#[derive(Debug, Clone)]
pub struct EntryBundlePlan {
    pub display_name: String,
    pub binary_source: PathBuf,
    pub binary_destination: PathBuf,
    pub linker_source: PathBuf,
    pub linker_destination: PathBuf,
    pub library_dirs: Vec<PathBuf>,
    pub requires_linker: bool,
}

/// 依赖闭包汇总结果，供装配/打包复用。
#[derive(Debug, Default, Clone)]
pub struct DependencyClosure {
    pub files: Vec<ResolvedFile>,
    pub entry_plans: Vec<EntryBundlePlan>,
    pub traced_files: Vec<TracedFile>,
}

impl DependencyClosure {
    pub fn add_file(mut self, file: ResolvedFile) -> Self {
        self.files.push(file);
        self
    }

    pub fn add_entry(mut self, entry: EntryBundlePlan) -> Self {
        self.entry_plans.push(entry);
        self
    }

    pub fn merge(&mut self, other: DependencyClosure) -> MergeReport {
        let mut by_destination: HashMap<PathBuf, String> = self
            .files
            .iter()
            .map(|file| (file.destination.clone(), file.digest.clone()))
            .collect();
        let mut seen_entry_names: HashSet<String> = self
            .entry_plans
            .iter()
            .map(|plan| plan.display_name.clone())
            .collect();
        let mut traced_set: HashSet<PathBuf> = self
            .traced_files
            .iter()
            .map(|file| file.resolved.clone())
            .collect();

        let mut report = MergeReport::default();

        for file in other.files {
            match by_destination.get(&file.destination) {
                Some(existing_digest) if existing_digest == &file.digest => {
                    report.reused_files += 1;
                }
                Some(existing_digest) => {
                    report.conflicts.push(MergeConflict {
                        destination: file.destination.clone(),
                        existing_digest: existing_digest.clone(),
                        incoming_digest: file.digest.clone(),
                        incoming_source: file.source.clone(),
                    });
                    continue;
                }
                None => {
                    by_destination.insert(file.destination.clone(), file.digest.clone());
                    report.added_files += 1;
                    self.files.push(file);
                }
            }
        }

        for plan in other.entry_plans {
            if seen_entry_names.insert(plan.display_name.clone()) {
                report.added_entries += 1;
                self.entry_plans.push(plan);
            } else {
                report.skipped_entries += 1;
            }
        }

        for traced in other.traced_files {
            if traced_set.insert(traced.resolved.clone()) {
                report.traced_added += 1;
                self.traced_files.push(traced);
            }
        }

        report
    }
}

#[derive(Debug, Clone)]
pub struct TracedFile {
    pub original: PathBuf,
    pub resolved: PathBuf,
    pub is_elf: bool,
}

#[derive(Debug, Default)]
pub struct MergeReport {
    pub added_files: usize,
    pub reused_files: usize,
    pub added_entries: usize,
    pub skipped_entries: usize,
    pub traced_added: usize,
    pub conflicts: Vec<MergeConflict>,
}

#[derive(Debug, Clone)]
pub struct MergeConflict {
    pub destination: PathBuf,
    pub existing_digest: String,
    pub incoming_digest: String,
    pub incoming_source: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_linux_triple() {
        let triple = TargetTriple::from_str("linux-x86_64").unwrap();
        assert_eq!(triple.as_str(), "linux-x86_64");
    }

    #[test]
    fn reject_unknown_target() {
        let err = TargetTriple::from_str("unknown").unwrap_err();
        assert!(err.to_string().contains("unsupported"));
    }

    #[test]
    fn merge_deduplicates_files_by_hash() {
        let mut base = DependencyClosure {
            files: vec![ResolvedFile::new("/a", "payload/bin/a", "hash-one")],
            entry_plans: Vec::new(),
            traced_files: vec![TracedFile {
                original: PathBuf::from("/etc/ssl/cert.pem"),
                resolved: PathBuf::from("/etc/ssl/cert.pem"),
                is_elf: false,
            }],
        };

        let other = DependencyClosure {
            files: vec![
                ResolvedFile::new("/a-copy", "payload/bin/a", "hash-one"),
                ResolvedFile::new("/b", "payload/lib/b", "hash-two"),
            ],
            entry_plans: Vec::new(),
            traced_files: vec![
                TracedFile {
                    original: PathBuf::from("/etc/ssl/cert.pem"),
                    resolved: PathBuf::from("/etc/ssl/cert.pem"),
                    is_elf: false,
                },
                TracedFile {
                    original: PathBuf::from("/tmp/runtime"),
                    resolved: PathBuf::from("/tmp/runtime"),
                    is_elf: false,
                },
            ],
        };

        let report = base.merge(other);
        assert_eq!(report.added_files, 1);
        assert_eq!(report.reused_files, 1);
        assert_eq!(report.traced_added, 1);
        assert!(report.conflicts.is_empty());
        assert_eq!(base.files.len(), 2);
        assert_eq!(base.traced_files.len(), 2);
    }

    #[test]
    fn merge_detects_conflicting_destinations() {
        let mut base = DependencyClosure {
            files: vec![ResolvedFile::new(
                "/bin/foo",
                "payload/bin/foo",
                "digest-foo",
            )],
            entry_plans: Vec::new(),
            traced_files: Vec::new(),
        };

        let other = DependencyClosure {
            files: vec![ResolvedFile::new(
                "/bin/foo-alt",
                "payload/bin/foo",
                "digest-bar",
            )],
            entry_plans: Vec::new(),
            traced_files: Vec::new(),
        };

        let report = base.merge(other);
        assert_eq!(report.conflicts.len(), 1);
        assert_eq!(base.files.len(), 1);
    }
}
