use std::path::PathBuf;

use sidebundle_core::{BundleSpec, DependencyClosure, ResolvedFile};

/// 负责把入口列表扩展成闭包文件集合的占位实现。
#[derive(Default)]
pub struct ClosureBuilder;

impl ClosureBuilder {
    pub fn new() -> Self {
        Self
    }

    /// 目前仅生成一个把入口二进制复制到 `bin/` 的伪闭包。
    /// 后续将替换为真正的依赖解析。
    pub fn build(&self, spec: &BundleSpec) -> DependencyClosure {
        let mut closure = DependencyClosure::default();
        for entry in spec.entries() {
            let destination = PathBuf::from("bin").join(&entry.display_name);
            closure.files.push(ResolvedFile::new(&entry.path, destination));
        }
        closure
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sidebundle_core::{BundleEntry, TargetTriple};

    #[test]
    fn closure_contains_bin_targets() {
        let spec = BundleSpec::new("demo", TargetTriple::linux_x86_64())
            .with_entry(BundleEntry::new("/usr/bin/echo", "echo"));
        let closure = ClosureBuilder::new().build(&spec);
        assert_eq!(closure.files.len(), 1);
        assert_eq!(closure.files[0].destination, PathBuf::from("bin/echo"));
    }
}
