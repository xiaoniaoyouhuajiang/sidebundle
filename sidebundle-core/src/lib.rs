use std::path::PathBuf;

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

    pub fn entries(&self) -> &[BundleEntry] {
        &self.entries
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

/// 依赖闭包汇总结果，供装配/打包复用。
#[derive(Debug, Default, Clone)]
pub struct DependencyClosure {
    pub files: Vec<ResolvedFile>,
}

impl DependencyClosure {
    pub fn add_file(mut self, file: ResolvedFile) -> Self {
        self.files.push(file);
        self
    }
}
