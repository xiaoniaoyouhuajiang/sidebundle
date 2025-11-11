mod elf;

pub use elf::{parse_elf_metadata, ElfMetadata, ElfParseError};

use std::error::Error;
use std::fmt::{self, Display};
use std::path::PathBuf;
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
pub struct BundleEntry {
    pub path: PathBuf,
    pub display_name: String,
}

impl BundleEntry {
    pub fn new(path: impl Into<PathBuf>, display_name: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            display_name: display_name.into(),
        }
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
}

impl ResolvedFile {
    pub fn new(source: impl Into<PathBuf>, destination: impl Into<PathBuf>) -> Self {
        Self {
            source: source.into(),
            destination: destination.into(),
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
}

/// 依赖闭包汇总结果，供装配/打包复用。
#[derive(Debug, Default, Clone)]
pub struct DependencyClosure {
    pub files: Vec<ResolvedFile>,
    pub entry_plans: Vec<EntryBundlePlan>,
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
}
