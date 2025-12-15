use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=SIDEBUNDLE_EMBED_BWRAP_BIN");

    // Only required when the feature is enabled.
    if env::var_os("CARGO_FEATURE_EMBEDDED_BWRAP").is_none() {
        return;
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
    let placeholder = out_dir.join("embedded-bwrap.placeholder");

    let (path, is_placeholder) = match env::var_os("SIDEBUNDLE_EMBED_BWRAP_BIN") {
        Some(bin) => {
            let path = PathBuf::from(bin);
            if !path.is_file() {
                panic!(
                    "SIDEBUNDLE_EMBED_BWRAP_BIN points to missing file: {}",
                    path.display()
                );
            }
            (path, false)
        }
        None => {
            // Allow `--all-features` builds in dev/CI without requiring the embed input.
            // The runtime will treat this as "embedded bwrap unavailable".
            fs::write(&placeholder, b"embedded-bwrap placeholder\n")
                .expect("failed to write embedded-bwrap placeholder");
            (placeholder, true)
        }
    };

    println!(
        "cargo:rustc-env=SIDEBUNDLE_EMBED_BWRAP_BIN_PATH={}",
        path.display()
    );
    println!(
        "cargo:rustc-env=SIDEBUNDLE_EMBED_BWRAP_IS_PLACEHOLDER={}",
        if is_placeholder { "1" } else { "0" }
    );
}
