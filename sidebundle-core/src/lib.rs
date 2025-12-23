mod elf;

pub use elf::{parse_elf_metadata, ElfMetadata, ElfParseError};

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
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

/// 运行模式，决定 launcher 如何进入打包环境。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    #[default]
    Host,
    Bwrap,
    Chroot,
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

/// 单条 AUXV 数据（键值对）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuxvEntry {
    pub key: u64,
    pub value: u64,
}

/// uname 快照，描述目标期望的内核/机器信息。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SystemInfo {
    pub sysname: String,
    pub nodename: String,
    pub release: String,
    pub version: String,
    pub machine: String,
}

/// 入口所需的运行期元数据（AUXV、环境变量等）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct RuntimeMetadata {
    #[serde(default)]
    pub auxv: Vec<AuxvEntry>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uname: Option<SystemInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub random: Option<[u8; 16]>,
}

impl RuntimeMetadata {
    pub fn is_empty(&self) -> bool {
        self.auxv.is_empty()
            && self.env.is_empty()
            && self.uname.is_none()
            && self.platform.is_none()
            && self.random.is_none()
    }
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
    pub run_mode: RunMode,
}

impl BundleSpec {
    pub fn new(name: impl Into<String>, target: TargetTriple) -> Self {
        Self {
            name: name.into(),
            target,
            entries: Vec::new(),
            run_mode: RunMode::default(),
        }
    }

    pub fn with_entry(mut self, entry: BundleEntry) -> Self {
        self.entries.push(entry);
        self
    }

    pub fn push_entry(&mut self, entry: BundleEntry) {
        self.entries.push(entry);
    }

    pub fn with_run_mode(mut self, run_mode: RunMode) -> Self {
        self.run_mode = run_mode;
        self
    }

    pub fn set_run_mode(&mut self, run_mode: RunMode) {
        self.run_mode = run_mode;
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

    pub fn run_mode(&self) -> RunMode {
        self.run_mode
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
pub enum EntryBundlePlan {
    Binary(BinaryEntryPlan),
    Script(ScriptEntryPlan),
}

#[derive(Debug, Clone)]
pub struct BinaryEntryPlan {
    pub display_name: String,
    pub binary_source: PathBuf,
    pub binary_destination: PathBuf,
    pub linker_source: PathBuf,
    pub linker_destination: PathBuf,
    pub library_dirs: Vec<PathBuf>,
    pub requires_linker: bool,
    pub origin: Origin,
    pub run_mode: Option<RunMode>,
}

#[derive(Debug, Clone)]
pub struct ScriptEntryPlan {
    pub display_name: String,
    pub script_source: PathBuf,
    pub script_destination: PathBuf,
    pub interpreter_source: PathBuf,
    pub interpreter_destination: PathBuf,
    pub linker_source: PathBuf,
    pub linker_destination: PathBuf,
    pub interpreter_args: Vec<String>,
    pub library_dirs: Vec<PathBuf>,
    pub requires_linker: bool,
    pub origin: Origin,
    pub run_mode: Option<RunMode>,
}

impl EntryBundlePlan {
    pub fn display_name(&self) -> &str {
        match self {
            EntryBundlePlan::Binary(plan) => &plan.display_name,
            EntryBundlePlan::Script(plan) => &plan.display_name,
        }
    }

    pub fn origin(&self) -> &Origin {
        match self {
            EntryBundlePlan::Binary(plan) => &plan.origin,
            EntryBundlePlan::Script(plan) => &plan.origin,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedSymlink {
    pub destination: PathBuf,
    pub bundle_target: PathBuf,
}

impl ResolvedSymlink {
    pub fn new(destination: impl Into<PathBuf>, bundle_target: impl Into<PathBuf>) -> Self {
        Self {
            destination: destination.into(),
            bundle_target: bundle_target.into(),
        }
    }
}

/// 依赖闭包汇总结果，供装配/打包复用。
#[derive(Debug, Default, Clone)]
pub struct DependencyClosure {
    pub files: Vec<ResolvedFile>,
    pub entry_plans: Vec<EntryBundlePlan>,
    pub traced_files: Vec<TracedFile>,
    pub runtime_aliases: HashMap<PathBuf, Vec<PathBuf>>,
    pub symlinks: Vec<ResolvedSymlink>,
    pub metadata: HashMap<Origin, RuntimeMetadata>,
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
            .map(|plan| plan.display_name().to_string())
            .collect();
        let mut traced_set: HashSet<PathBuf> = self
            .traced_files
            .iter()
            .map(|file| file.resolved.clone())
            .collect();
        let mut symlink_map: HashMap<PathBuf, PathBuf> = self
            .symlinks
            .iter()
            .map(|link| (link.destination.clone(), link.bundle_target.clone()))
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
            if seen_entry_names.insert(plan.display_name().to_string()) {
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

        for link in other.symlinks {
            match symlink_map.get(&link.destination) {
                Some(existing_target) if existing_target == &link.bundle_target => {}
                Some(_) => continue,
                None => {
                    symlink_map.insert(link.destination.clone(), link.bundle_target.clone());
                    self.symlinks.push(link);
                }
            }
        }

        for (source, aliases) in other.runtime_aliases {
            let entry = self.runtime_aliases.entry(source).or_default();
            entry.extend(aliases);
        }
        for (origin, snapshot) in other.metadata {
            self.metadata.entry(origin).or_insert(snapshot);
        }

        report
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TraceAccess(u8);

impl TraceAccess {
    pub const OPEN: TraceAccess = TraceAccess(0b0001);
    pub const STAT: TraceAccess = TraceAccess(0b0010);
    pub const LINK: TraceAccess = TraceAccess(0b0100);
    pub const EXEC: TraceAccess = TraceAccess(0b1000);

    pub const fn empty() -> TraceAccess {
        TraceAccess(0)
    }

    pub fn contains(self, other: TraceAccess) -> bool {
        (self.0 & other.0) == other.0
    }

    pub fn insert(&mut self, other: TraceAccess) {
        self.0 |= other.0;
    }

    pub fn union(self, other: TraceAccess) -> TraceAccess {
        TraceAccess(self.0 | other.0)
    }
}

impl Default for TraceAccess {
    fn default() -> Self {
        TraceAccess::empty()
    }
}

#[derive(Debug, Clone)]
pub struct TracedFile {
    pub original: PathBuf,
    pub resolved: PathBuf,
    pub is_elf: bool,
    pub access: TraceAccess,
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
                access: TraceAccess::OPEN,
            }],
            runtime_aliases: HashMap::new(),
            symlinks: Vec::new(),
            metadata: HashMap::new(),
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
                    access: TraceAccess::OPEN,
                },
                TracedFile {
                    original: PathBuf::from("/tmp/runtime"),
                    resolved: PathBuf::from("/tmp/runtime"),
                    is_elf: false,
                    access: TraceAccess::OPEN,
                },
            ],
            runtime_aliases: HashMap::new(),
            symlinks: Vec::new(),
            metadata: HashMap::new(),
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
            runtime_aliases: HashMap::new(),
            symlinks: Vec::new(),
            metadata: HashMap::new(),
        };

        let other = DependencyClosure {
            files: vec![ResolvedFile::new(
                "/bin/foo-alt",
                "payload/bin/foo",
                "digest-bar",
            )],
            entry_plans: Vec::new(),
            traced_files: Vec::new(),
            runtime_aliases: HashMap::new(),
            symlinks: Vec::new(),
            metadata: HashMap::new(),
        };

        let report = base.merge(other);
        assert_eq!(report.conflicts.len(), 1);
        assert_eq!(base.files.len(), 1);
    }
}
