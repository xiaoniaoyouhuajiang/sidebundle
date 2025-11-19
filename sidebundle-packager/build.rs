use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let workspace_root = manifest
        .parent()
        .expect("failed to resolve workspace root")
        .to_path_buf();
    let target = env::var("TARGET").expect("target triple");
    let profile = env::var("PROFILE").expect("profile");
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let target_dir = env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root.join("target"));
    let launcher_target_dir = target_dir.join("sidebundle-launcher-build");
    let launcher_manifest = workspace_root
        .join("sidebundle-launcher")
        .join("Cargo.toml");

    let mut command = Command::new(&cargo);
    command
        .arg("build")
        .arg("--manifest-path")
        .arg(&launcher_manifest)
        .arg("--target")
        .arg(&target)
        .arg("--target-dir")
        .arg(&launcher_target_dir);
    if profile == "release" {
        command.arg("--release");
    } else if profile != "debug" {
        command.arg("--profile").arg(&profile);
    }
    if env::var("CARGO_NET_OFFLINE").map_or(false, |v| v == "true") {
        command.arg("--offline");
    }
    let status = command
        .status()
        .expect("failed to invoke cargo for launcher build");
    if !status.success() {
        panic!("failed to compile sidebundle-launcher");
    }

    let profile_dir = if profile == "release" {
        "release"
    } else {
        profile.as_str()
    };
    let built_path = launcher_target_dir
        .join(&target)
        .join(profile_dir)
        .join("sidebundle-launcher");
    copy_launcher(&built_path, &out_dir.join("sidebundle-launcher"));

    if let Some(src_dir) = launcher_manifest.parent() {
        println!("cargo:rerun-if-changed={}", src_dir.display());
    }
    println!(
        "cargo:rustc-env=SIDEBUNDLE_LAUNCHER_BIN={}",
        out_dir.join("sidebundle-launcher").display()
    );
}

fn copy_launcher(source: &Path, dest: &Path) {
    fs::copy(source, dest).unwrap_or_else(|err| {
        panic!(
            "failed to copy launcher binary from {} to {}: {err}",
            source.display(),
            dest.display()
        )
    });
}
