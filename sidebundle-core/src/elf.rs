use std::fs;
use std::path::{Path, PathBuf};

use goblin::Object;

/// ELF 元信息，供闭包构建参考。
#[derive(Debug, Clone)]
pub struct ElfMetadata {
    pub interpreter: Option<PathBuf>,
    pub needed: Vec<String>,
    pub rpaths: Vec<String>,
    pub runpaths: Vec<String>,
    pub soname: Option<String>,
}

/// 解析失败时的错误类型。
#[derive(Debug)]
pub enum ElfParseError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse {
        path: PathBuf,
        source: goblin::error::Error,
    },
    NotElf {
        path: PathBuf,
    },
}

impl std::fmt::Display for ElfParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ElfParseError::Io { path, source } => {
                write!(f, "failed to read ELF {}: {}", path.display(), source)
            }
            ElfParseError::Parse { path, source } => {
                write!(f, "failed to parse ELF {}: {}", path.display(), source)
            }
            ElfParseError::NotElf { path } => {
                write!(f, "{} is not an ELF binary", path.display())
            }
        }
    }
}

impl std::error::Error for ElfParseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ElfParseError::Io { source, .. } => Some(source),
            ElfParseError::Parse { source, .. } => Some(source),
            ElfParseError::NotElf { .. } => None,
        }
    }
}

/// 解析给定路径的 ELF 文件，并返回元信息。
pub fn parse_elf_metadata(path: &Path) -> Result<ElfMetadata, ElfParseError> {
    let data = fs::read(path).map_err(|source| ElfParseError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    let object = Object::parse(&data).map_err(|source| ElfParseError::Parse {
        path: path.to_path_buf(),
        source,
    })?;

    let elf = match object {
        Object::Elf(elf) => elf,
        _ => {
            return Err(ElfParseError::NotElf {
                path: path.to_path_buf(),
            })
        }
    };

    Ok(ElfMetadata {
        interpreter: elf.interpreter.map(PathBuf::from),
        needed: elf.libraries.iter().map(|lib| lib.to_string()).collect(),
        rpaths: elf.rpaths.iter().map(|r| r.to_string()).collect(),
        runpaths: elf.runpaths.iter().map(|r| r.to_string()).collect(),
        soname: elf.soname.map(|s| s.to_string()),
    })
}
