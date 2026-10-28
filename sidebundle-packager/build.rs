use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=SIDEBUNDLE_EMBED_BWRAP_BIN");
    println!("cargo:rerun-if-env-changed=SIDEBUNDLE_LAUNCHER_TARGET");
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let workspace_root = manifest
        .parent()
        .expect("failed to resolve workspace root")
        .to_path_buf();
    let host_target = env::var("TARGET").expect("target triple");
    let target = launcher_target(&host_target);
    let profile = env::var("PROFILE").expect("profile");
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let target_dir = env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root.join("target"));
    let launcher_target_dir = target_dir.join("sidebundle-launcher-build");
    let launcher_manifest = workspace_root
        .join("sidebundle-launcher")
        .join("Cargo.toml");
    let shim_target_dir = target_dir.join("sidebundle-shim-build");
    let shim_manifest = workspace_root.join("sidebundle-shim").join("Cargo.toml");

    let mut command = Command::new(&cargo);
    command
        .arg("build")
        .arg("--manifest-path")
        .arg(&launcher_manifest)
        .arg("--target")
        .arg(&target)
        .arg("--target-dir")
        .arg(&launcher_target_dir);
    configure_musl_linker(&mut command, &target);
    if env::var_os("SIDEBUNDLE_EMBED_BWRAP_BIN").is_some() {
        command.arg("--features").arg("embedded-bwrap");
    }
    if profile == "release" {
        command.arg("--release");
    } else if profile != "debug" {
        command.arg("--profile").arg(&profile);
    }
    if env::var("CARGO_NET_OFFLINE").is_ok_and(|v| v == "true") {
        command.arg("--offline");
    }
    let status = command
        .status()
        .expect("failed to invoke cargo for launcher build");
    if !status.success() {
        panic!("failed to compile sidebundle-launcher");
    }

    let mut shim_build = Command::new(&cargo);
    shim_build
        .arg("build")
        .arg("--manifest-path")
        .arg(&shim_manifest)
        .arg("--target")
        .arg(&target)
        .arg("--target-dir")
        .arg(&shim_target_dir);
    configure_musl_linker(&mut shim_build, &target);
    if profile == "release" {
        shim_build.arg("--release");
    } else if profile != "debug" {
        shim_build.arg("--profile").arg(&profile);
    }
    if env::var("CARGO_NET_OFFLINE").is_ok_and(|v| v == "true") {
        shim_build.arg("--offline");
    }
    let shim_status = shim_build
        .status()
        .expect("failed to invoke cargo for shim build");
    if !shim_status.success() {
        panic!("failed to compile sidebundle-shim");
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
    let launcher_out = out_dir.join("sidebundle-launcher");
    copy_launcher(&built_path, &launcher_out);
    if elf_has_pt_interp(&launcher_out) {
        println!("cargo:rustc-env=SIDEBUNDLE_LAUNCHER_STATIC=0");
        println!(
            "cargo:warning=embedded sidebundle-launcher is dynamically linked ({target}) and is not relocatable: bundles fail on older glibc (centos:7, ubuntu 18.04; matrix LAUNCH_FAIL / P0-3). Install `rustup target add {}` or set SIDEBUNDLE_LAUNCHER_TARGET. See docs/install.md",
            gnu_to_musl(&host_target).unwrap_or_else(|| target.clone())
        );
    } else {
        println!("cargo:rustc-env=SIDEBUNDLE_LAUNCHER_STATIC=1");
    }
    let shim_built_path = shim_target_dir
        .join(&target)
        .join(profile_dir)
        .join("sidebundle-shim");
    copy_launcher(&shim_built_path, &out_dir.join("sidebundle-shim"));

    if let Some(src_dir) = launcher_manifest.parent() {
        println!("cargo:rerun-if-changed={}", src_dir.display());
    }
    if let Some(src_dir) = shim_manifest.parent() {
        println!("cargo:rerun-if-changed={}", src_dir.display());
    }
    println!(
        "cargo:rustc-env=SIDEBUNDLE_LAUNCHER_BIN={}",
        out_dir.join("sidebundle-launcher").display()
    );
    println!(
        "cargo:rustc-env=SIDEBUNDLE_SHIM_BIN={}",
        out_dir.join("sidebundle-shim").display()
    );
}

/// Prefer a musl static launcher/shim so relocatable bundles do not embed a glibc
/// dynamic linker that fails on centos:7 (verification matrix: true gnu / P0-3).
fn launcher_target(host_target: &str) -> String {
    if let Ok(explicit) = env::var("SIDEBUNDLE_LAUNCHER_TARGET") {
        let explicit = explicit.trim();
        if !explicit.is_empty() {
            return explicit.to_string();
        }
    }
    if host_target.contains("musl") || !host_target.contains("linux") {
        return host_target.to_string();
    }
    let Some(musl) = gnu_to_musl(host_target) else {
        return host_target.to_string();
    };
    if musl_toolchain_ready(&musl) {
        println!(
            "cargo:warning=building embedded launcher and shim for {musl} so bundles stay relocatable on older glibc"
        );
        musl
    } else {
        println!(
            "cargo:warning=musl target {musl} is not installed; embedded launcher will be glibc-dynamic and fail on centos:7 (P0-3). Run `rustup target add {musl}`"
        );
        host_target.to_string()
    }
}

fn gnu_to_musl(host_target: &str) -> Option<String> {
    if host_target.contains("unknown-linux-gnu") {
        Some(host_target.replace("unknown-linux-gnu", "unknown-linux-musl"))
    } else {
        host_target
            .strip_suffix("-linux-gnu")
            .map(|stripped| format!("{stripped}-linux-musl"))
    }
}

fn musl_toolchain_ready(target: &str) -> bool {
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let output = Command::new(rustc)
        .args(["--print", "target-libdir", "--target", target])
        .output();
    let Ok(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let dir = String::from_utf8_lossy(&output.stdout);
    let dir = Path::new(dir.trim());
    dir_has_rlib(dir, "libcore") && rust_lld_path().is_some()
}

fn dir_has_rlib(dir: &Path, prefix: &str) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        name.starts_with(prefix) && name.ends_with(".rlib")
    })
}

fn rust_lld_path() -> Option<PathBuf> {
    if Command::new("rust-lld")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
    {
        return Some(PathBuf::from("rust-lld"));
    }
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let sysroot = Command::new(rustc)
        .args(["--print", "sysroot"])
        .output()
        .ok()?;
    if !sysroot.status.success() {
        return None;
    }
    let sysroot = PathBuf::from(String::from_utf8_lossy(&sysroot.stdout).trim().to_string());
    let host = env::var("HOST").unwrap_or_default();
    let candidate = sysroot.join("lib/rustlib").join(host).join("bin/rust-lld");
    if candidate.is_file() {
        Some(candidate)
    } else {
        None
    }
}

fn configure_musl_linker(command: &mut Command, target: &str) {
    if !target.contains("musl") {
        return;
    }
    let key = format!(
        "CARGO_TARGET_{}_LINKER",
        target.replace('-', "_").to_ascii_uppercase()
    );
    if env::var_os(&key).is_some() {
        return;
    }
    if let Some(linker) = rust_lld_path() {
        command.env(key, linker);
    }
}

fn elf_has_pt_interp(path: &Path) -> bool {
    let Ok(data) = fs::read(path) else {
        return true;
    };
    if data.len() < 64 || &data[0..4] != b"\x7fELF" || data[4] != 2 || data[5] != 1 {
        return true;
    }
    let e_phoff = u64::from_le_bytes(data[32..40].try_into().unwrap_or([0; 8]));
    let e_phentsize = u16::from_le_bytes(data[54..56].try_into().unwrap_or([0; 2]));
    let e_phnum = u16::from_le_bytes(data[56..58].try_into().unwrap_or([0; 2]));
    if e_phentsize < 4 {
        return true;
    }
    for index in 0..e_phnum {
        let start = e_phoff as usize + index as usize * e_phentsize as usize;
        let Some(header) = data.get(start..start + 4) else {
            break;
        };
        let kind = u32::from_le_bytes(header.try_into().unwrap_or([0; 4]));
        if kind == 3 {
            return true;
        }
    }
    false
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
