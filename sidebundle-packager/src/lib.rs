use std::error::Error;
use std::fmt::{Display, Formatter};

use sidebundle_core::{BundleSpec, DependencyClosure};

/// 负责把闭包内容写入输出目录的占位实现。
#[derive(Default)]
pub struct Packager;

impl Packager {
    pub fn new() -> Self {
        Self
    }

    /// 目前仅校验闭包非空，并返回结构化错误；后续会接入真实的打包逻辑。
    pub fn emit(
        &self,
        spec: &BundleSpec,
        closure: &DependencyClosure,
    ) -> Result<(), PackagerError> {
        if closure.files.is_empty() {
            return Err(PackagerError::EmptyClosure(spec.name.clone()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub enum PackagerError {
    EmptyClosure(String),
}

impl Display for PackagerError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            PackagerError::EmptyClosure(name) => {
                write!(f, "bundle `{name}` has no files to package")
            }
        }
    }
}

impl Error for PackagerError {}

#[cfg(test)]
mod tests {
    use super::*;
    use sidebundle_core::{BundleEntry, TargetTriple};

    #[test]
    fn emits_when_closure_non_empty() {
        let spec = BundleSpec::new("demo", TargetTriple::linux_x86_64())
            .with_entry(BundleEntry::new("/bin/echo", "echo"));
        let closure = DependencyClosure::default();
        let packager = Packager::new();
        let err = packager.emit(&spec, &closure).unwrap_err();
        assert!(format!("{err}").contains("no files"));
    }
}
