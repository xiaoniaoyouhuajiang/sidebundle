use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let target = env::var("TARGET").expect("target");
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let source = manifest.join("src").join("launcher_template.rs");
    let output = out_dir.join("sidebundle-launcher");

    let status = Command::new(&rustc)
        .arg("--crate-type")
        .arg("bin")
        .arg("--edition=2021")
        .arg("-C")
        .arg("opt-level=z")
        .arg("-C")
        .arg("strip=symbols")
        .arg("-C")
        .arg("panic=abort")
        .arg("--target")
        .arg(&target)
        .arg(&source)
        .arg("-o")
        .arg(&output)
        .status()
        .expect("failed to invoke rustc for launcher");

    if !status.success() {
        panic!("failed to compile launcher template");
    }

    println!("cargo:rerun-if-changed={}", source.display());
    println!(
        "cargo:rustc-env=SIDEBUNDLE_LAUNCHER_BIN={}",
        output.display()
    );
}
