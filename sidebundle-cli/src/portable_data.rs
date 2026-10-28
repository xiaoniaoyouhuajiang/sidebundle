//! Copy well-known data files into the payload and record bundle-root-relative env defaults.
//!
//! Verification (2026-10-28): curl-host failed on centos:7 / ubuntu 18.04 because the CA bundle
//! stayed on the host absolute path, and tesseract shipped tessdata but still read the image path.
//! ImageMagick 6 loads coders through libltdl (`.la` next to the module) and reads
//! `delegates.xml` from `MAGICK_CONFIGURE_PATH`. Trace of `-version` does not open that tree.
//! Values use `${SIDEBUNDLE_ROOT}` so the launcher can expand them for any mount point.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use log::{debug, info};
use sha2::{Digest, Sha256};
use sidebundle_core::{BinaryEntryPlan, DependencyClosure, EntryBundlePlan, Origin, ResolvedFile};
use walkdir::WalkDir;

const CA_CANDIDATES: &[&str] = &[
    "etc/ssl/certs/ca-certificates.crt",
    "etc/ssl/cert.pem",
    "etc/pki/tls/certs/ca-bundle.crt",
    "etc/ssl/certs/ca-bundle.crt",
];

pub fn inject_portable_data(
    closure: &mut DependencyClosure,
    origin_roots: &[(Origin, PathBuf)],
) -> Result<()> {
    for (origin, root) in origin_roots {
        if origin_needs_ca(closure, origin) {
            if let Some(rel) = find_existing_rel(root, CA_CANDIDATES) {
                let source = root.join(rel);
                add_file(closure, &source, rel).with_context(|| {
                    format!(
                        "failed to bundle CA file {} for {origin:?}",
                        source.display()
                    )
                })?;
                let portable = portable_value(rel);
                let meta = closure.metadata.entry(origin.clone()).or_default();
                meta.env_defaults
                    .entry("SSL_CERT_FILE".into())
                    .or_insert_with(|| portable.clone());
                meta.env_defaults
                    .entry("CURL_CA_BUNDLE".into())
                    .or_insert_with(|| portable.clone());
                meta.env_defaults
                    .entry("REQUESTS_CA_BUNDLE".into())
                    .or_insert(portable);
                info!(
                    "portable data: SSL_CERT_FILE for {origin:?} -> ${{SIDEBUNDLE_ROOT}}/payload/{rel}"
                );
            } else {
                debug!(
                    "portable data: {origin:?} looks SSL-related but no CA bundle under {}",
                    root.display()
                );
            }
        }
        if origin_needs_tessdata(closure, origin) {
            if let Some(rel_dir) = find_tessdata_rel(root) {
                let source = root.join(&rel_dir);
                add_tree(closure, &source, &rel_dir).with_context(|| {
                    format!(
                        "failed to bundle tessdata {} for {origin:?}",
                        source.display()
                    )
                })?;
                let portable = portable_value(&rel_dir);
                closure
                    .metadata
                    .entry(origin.clone())
                    .or_default()
                    .env_defaults
                    .entry("TESSDATA_PREFIX".into())
                    .or_insert(portable);
                info!(
                    "portable data: TESSDATA_PREFIX for {origin:?} -> ${{SIDEBUNDLE_ROOT}}/payload/{rel_dir}"
                );
            }
        }
        if origin_needs_magick(closure, origin) {
            inject_magick_config(closure, origin, root)?;
        }
    }
    Ok(())
}

fn inject_magick_config(
    closure: &mut DependencyClosure,
    origin: &Origin,
    root: &Path,
) -> Result<()> {
    let configs = magick_config_rels(root);
    for rel in &configs {
        let source = root.join(rel);
        add_tree(closure, &source, rel).with_context(|| {
            format!(
                "failed to bundle ImageMagick config {} for {origin:?}",
                source.display()
            )
        })?;
    }
    let configure = configs
        .iter()
        .find(|rel| root.join(rel).join("delegates.xml").is_file())
        .cloned()
        .or_else(|| configs.first().cloned());
    let coders = find_magick_module_rel(root, "coders");
    let filters = find_magick_module_rel(root, "filters");
    if configure.is_none() && coders.is_none() && filters.is_none() {
        return Ok(());
    }
    let meta = closure.metadata.entry(origin.clone()).or_default();
    if let Some(rel) = configure {
        meta.env_defaults
            .entry("MAGICK_CONFIGURE_PATH".into())
            .or_insert_with(|| portable_value(&rel));
        info!(
            "portable data: MAGICK_CONFIGURE_PATH for {origin:?} -> ${{SIDEBUNDLE_ROOT}}/payload/{rel}"
        );
    }
    if let Some(rel) = coders {
        meta.env_defaults
            .entry("MAGICK_CODER_MODULE_PATH".into())
            .or_insert_with(|| portable_value(&rel));
        info!(
            "portable data: MAGICK_CODER_MODULE_PATH for {origin:?} -> ${{SIDEBUNDLE_ROOT}}/payload/{rel}"
        );
    }
    if let Some(rel) = filters {
        meta.env_defaults
            .entry("MAGICK_FILTER_MODULE_PATH".into())
            .or_insert_with(|| portable_value(&rel));
        info!(
            "portable data: MAGICK_FILTER_MODULE_PATH for {origin:?} -> ${{SIDEBUNDLE_ROOT}}/payload/{rel}"
        );
    }
    Ok(())
}

fn portable_value(rel: &str) -> String {
    format!("${{SIDEBUNDLE_ROOT}}/payload/{rel}")
}

fn origin_needs_ca(closure: &DependencyClosure, origin: &Origin) -> bool {
    if closure
        .entry_plans
        .iter()
        .filter(|plan| plan.origin() == origin)
        .any(|plan| is_curl_name(plan.display_name()) || plan_links_ssl(plan))
    {
        return true;
    }
    closure.files.iter().any(|file| {
        file_belongs_to_origin(file, origin, closure)
            && is_ssl_library_name(&file_name(&file.destination))
    })
}

fn origin_needs_tessdata(closure: &DependencyClosure, origin: &Origin) -> bool {
    closure
        .entry_plans
        .iter()
        .filter(|plan| plan.origin() == origin)
        .any(|plan| is_tesseract_name(plan.display_name()) || plan_is_tesseract_binary(plan))
}

fn origin_needs_magick(closure: &DependencyClosure, origin: &Origin) -> bool {
    closure
        .entry_plans
        .iter()
        .filter(|plan| plan.origin() == origin)
        .any(plan_is_magick)
}

fn plan_is_magick(plan: &EntryBundlePlan) -> bool {
    if is_magick_tool_name(plan.display_name()) {
        return true;
    }
    match plan {
        EntryBundlePlan::Binary(plan) => {
            path_is_magick(&plan.binary_source) || path_is_magick(&plan.binary_destination)
        }
        EntryBundlePlan::Script(plan) => {
            path_is_magick(&plan.script_source) || path_is_magick(&plan.script_destination)
        }
    }
}

fn path_is_magick(path: &Path) -> bool {
    let name = file_name(path);
    is_magick_tool_name(&name) || path.to_string_lossy().contains("ImageMagick")
}

fn is_magick_tool_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "convert"
        || lower == "magick"
        || lower.starts_with("convert-im")
        || lower.starts_with("magick-")
}

fn plan_links_ssl(plan: &EntryBundlePlan) -> bool {
    match plan {
        EntryBundlePlan::Binary(plan) => {
            path_is_ssl(&plan.binary_source) || path_is_ssl(&plan.binary_destination)
        }
        EntryBundlePlan::Script(plan) => {
            path_is_ssl(&plan.interpreter_source) || path_is_ssl(&plan.script_source)
        }
    }
}

fn plan_is_tesseract_binary(plan: &EntryBundlePlan) -> bool {
    match plan {
        EntryBundlePlan::Binary(BinaryEntryPlan {
            binary_source,
            binary_destination,
            ..
        }) => {
            is_tesseract_name(&file_name(binary_source))
                || is_tesseract_name(&file_name(binary_destination))
        }
        EntryBundlePlan::Script(_) => false,
    }
}

fn file_belongs_to_origin(
    file: &ResolvedFile,
    origin: &Origin,
    closure: &DependencyClosure,
) -> bool {
    closure
        .entry_plans
        .iter()
        .any(|plan| plan.origin() == origin)
        && (path_is_ssl(&file.source) || path_is_ssl(&file.destination))
}

fn path_is_ssl(path: &Path) -> bool {
    is_ssl_library_name(&file_name(path)) || is_curl_name(&file_name(path))
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_string()
}

fn is_curl_name(name: &str) -> bool {
    name == "curl" || name.starts_with("curl-")
}

fn is_tesseract_name(name: &str) -> bool {
    name == "tesseract"
}

fn is_ssl_library_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("libssl.so")
        || lower.starts_with("libcrypto.so")
        || lower.starts_with("libcurl.so")
}

fn find_existing_rel<'a>(root: &Path, candidates: &[&'a str]) -> Option<&'a str> {
    candidates
        .iter()
        .copied()
        .find(|rel| root.join(rel).is_file())
}

fn find_tessdata_rel(root: &Path) -> Option<String> {
    let mut candidates = vec![
        PathBuf::from("usr/share/tessdata"),
        PathBuf::from("usr/local/share/tessdata"),
        PathBuf::from("usr/share/tesseract-ocr/tessdata"),
    ];
    let version_root = root.join("usr/share/tesseract-ocr");
    if let Ok(entries) = fs::read_dir(&version_root) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            candidates.push(
                PathBuf::from("usr/share/tesseract-ocr")
                    .join(&name)
                    .join("tessdata"),
            );
            candidates.push(PathBuf::from("usr/share/tesseract-ocr").join(name));
        }
    }
    let mut fallback = None;
    for rel in candidates {
        let dir = root.join(&rel);
        if !dir.is_dir() {
            continue;
        }
        if dir.join("eng.traineddata").is_file() {
            return Some(rel.to_string_lossy().replace('\\', "/"));
        }
        if fallback.is_none() && dir_has_traineddata(&dir) {
            fallback = Some(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    fallback
}

fn dir_has_traineddata(dir: &Path) -> bool {
    fs::read_dir(dir).ok().is_some_and(|mut entries| {
        entries.any(|entry| {
            entry
                .ok()
                .and_then(|ent| {
                    ent.file_name()
                        .to_str()
                        .map(|name| name.ends_with(".traineddata"))
                })
                .unwrap_or(false)
        })
    })
}

fn magick_config_rels(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for rel in ["etc/ImageMagick-6", "etc/ImageMagick-7"] {
        if root.join(rel).is_dir() {
            found.push(rel.to_string());
        }
    }
    let mut extra = Vec::new();
    for base in ["usr/lib", "usr/lib64", "usr/local/lib", "lib", "lib64"] {
        walk_magick_dirs(&root.join(base), 0, &mut |dir| {
            let name = dir.file_name().and_then(|name| name.to_str()).unwrap_or("");
            if !name.starts_with("config-Q") || !dir.join("delegates.xml").is_file() {
                return;
            }
            if !dir.to_string_lossy().contains("ImageMagick") {
                return;
            }
            if let Ok(rel) = dir.strip_prefix(root) {
                extra.push(rel.to_string_lossy().replace('\\', "/"));
            }
        });
    }
    extra.sort();
    extra.dedup();
    found.extend(extra);
    found
}

fn find_magick_module_rel(root: &Path, kind: &str) -> Option<String> {
    let mut preferred = None;
    let mut fallback = None;
    for base in ["usr/lib", "usr/lib64", "usr/local/lib", "lib", "lib64"] {
        walk_magick_dirs(&root.join(base), 0, &mut |dir| {
            if dir.file_name().and_then(|name| name.to_str()) != Some(kind) {
                return;
            }
            if !dir.to_string_lossy().contains("ImageMagick") {
                return;
            }
            let Ok(rel) = dir.strip_prefix(root) else {
                return;
            };
            let rel = rel.to_string_lossy().replace('\\', "/");
            if dir.join("png.la").is_file() || dir.join("png.so").is_file() {
                if preferred.is_none() {
                    preferred = Some(rel);
                }
            } else if fallback.is_none() {
                fallback = Some(rel);
            }
        });
    }
    preferred.or(fallback)
}

fn walk_magick_dirs(dir: &Path, depth: usize, visit: &mut dyn FnMut(&Path)) {
    if depth > 8 || !dir.is_dir() {
        return;
    }
    visit(dir);
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            walk_magick_dirs(&path, depth + 1, visit);
        }
    }
}

fn add_file(closure: &mut DependencyClosure, source: &Path, rel: &str) -> Result<()> {
    let destination = payload_dest(rel);
    if closure
        .files
        .iter()
        .any(|file| file.destination == destination)
    {
        return Ok(());
    }
    let digest = sha256_file(source)?;
    closure
        .files
        .push(ResolvedFile::new(source, destination, digest));
    Ok(())
}

fn add_tree(closure: &mut DependencyClosure, source_dir: &Path, rel_dir: &str) -> Result<()> {
    for entry in WalkDir::new(source_dir).follow_links(true).into_iter() {
        let entry = entry.with_context(|| format!("failed to walk {}", source_dir.display()))?;
        let meta = fs::metadata(entry.path())
            .with_context(|| format!("failed to stat {}", entry.path().display()))?;
        if !meta.is_file() {
            continue;
        }
        if let Ok(canonical) = fs::canonicalize(entry.path()) {
            if !canonical.starts_with(source_dir) {
                debug!(
                    "portable data: skipping {} (escapes {})",
                    canonical.display(),
                    source_dir.display()
                );
                continue;
            }
        }
        let rel_from_dir = entry.path().strip_prefix(source_dir).with_context(|| {
            format!(
                "failed to strip {} from {}",
                source_dir.display(),
                entry.path().display()
            )
        })?;
        let mut rel = PathBuf::from(rel_dir);
        rel.push(rel_from_dir);
        let rel = rel.to_string_lossy().replace('\\', "/");
        add_file(closure, entry.path(), &rel)?;
    }
    Ok(())
}

fn payload_dest(rel: &str) -> PathBuf {
    let mut dest = PathBuf::from("payload");
    for component in Path::new(rel).components() {
        dest.push(component);
    }
    dest
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)
        .with_context(|| format!("failed to open {} for hashing", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = std::io::Read::read(&mut file, &mut buffer)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sidebundle_core::{BinaryEntryPlan, ScriptEntryPlan};

    fn binary_plan(name: &str, origin: Origin) -> EntryBundlePlan {
        EntryBundlePlan::Binary(BinaryEntryPlan {
            display_name: name.into(),
            binary_source: PathBuf::from(format!("/usr/bin/{name}")),
            binary_destination: PathBuf::from(format!("payload/usr/bin/{name}")),
            linker_source: PathBuf::from("/lib64/ld-linux-x86-64.so.2"),
            linker_destination: PathBuf::from("payload/lib64/ld-linux-x86-64.so.2"),
            library_dirs: Vec::new(),
            requires_linker: true,
            origin,
            run_mode: None,
        })
    }

    fn script_plan(name: &str, origin: Origin) -> EntryBundlePlan {
        EntryBundlePlan::Script(ScriptEntryPlan {
            display_name: name.into(),
            script_source: PathBuf::from(format!("/usr/bin/{name}")),
            script_destination: PathBuf::from(format!("payload/usr/bin/{name}")),
            interpreter_source: PathBuf::from("/usr/bin/perl"),
            interpreter_destination: PathBuf::from("payload/usr/bin/perl"),
            linker_source: PathBuf::new(),
            linker_destination: PathBuf::new(),
            interpreter_args: Vec::new(),
            library_dirs: Vec::new(),
            requires_linker: false,
            origin,
            run_mode: None,
        })
    }

    #[test]
    fn curl_gets_ca_bundle_and_portable_env() {
        let root = tempfile::tempdir().unwrap();
        let rel = "etc/ssl/certs/ca-certificates.crt";
        let ca = root.path().join(rel);
        fs::create_dir_all(ca.parent().unwrap()).unwrap();
        fs::write(&ca, b"ca-data").unwrap();

        let origin = Origin::Image("curl".into());
        let mut closure =
            DependencyClosure::default().add_entry(binary_plan("curl", origin.clone()));
        inject_portable_data(&mut closure, &[(origin.clone(), root.path().to_path_buf())]).unwrap();

        assert!(closure.files.iter().any(|file| {
            file.destination == Path::new("payload/etc/ssl/certs/ca-certificates.crt")
        }));
        let meta = closure.metadata.get(&origin).unwrap();
        assert_eq!(
            meta.env_defaults.get("SSL_CERT_FILE").map(String::as_str),
            Some("${SIDEBUNDLE_ROOT}/payload/etc/ssl/certs/ca-certificates.crt")
        );
        assert_eq!(
            meta.env_defaults.get("CURL_CA_BUNDLE").map(String::as_str),
            Some("${SIDEBUNDLE_ROOT}/payload/etc/ssl/certs/ca-certificates.crt")
        );
    }

    #[test]
    fn unrelated_entry_does_not_copy_ca() {
        let root = tempfile::tempdir().unwrap();
        let ca = root.path().join("etc/ssl/certs/ca-certificates.crt");
        fs::create_dir_all(ca.parent().unwrap()).unwrap();
        fs::write(&ca, b"ca-data").unwrap();
        let origin = Origin::Host;
        let mut closure =
            DependencyClosure::default().add_entry(binary_plan("true", origin.clone()));
        inject_portable_data(&mut closure, &[(origin, root.path().to_path_buf())]).unwrap();
        assert!(closure.files.is_empty());
        assert!(closure.metadata.is_empty());
    }

    #[test]
    fn tesseract_copies_traineddata_and_sets_prefix() {
        let root = tempfile::tempdir().unwrap();
        let rel = "usr/share/tesseract-ocr/5/tessdata";
        let dir = root.path().join(rel);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("eng.traineddata"), b"eng").unwrap();
        let origin = Origin::Image("tess".into());
        let mut closure =
            DependencyClosure::default().add_entry(binary_plan("tesseract", origin.clone()));
        inject_portable_data(&mut closure, &[(origin.clone(), root.path().to_path_buf())]).unwrap();
        assert!(closure.files.iter().any(|file| {
            file.destination
                == Path::new("payload/usr/share/tesseract-ocr/5/tessdata/eng.traineddata")
        }));
        let prefix = closure
            .metadata
            .get(&origin)
            .unwrap()
            .env_defaults
            .get("TESSDATA_PREFIX")
            .cloned()
            .unwrap();
        assert_eq!(
            prefix,
            "${SIDEBUNDLE_ROOT}/payload/usr/share/tesseract-ocr/5/tessdata"
        );
    }

    #[test]
    fn script_named_curl_still_triggers_ca() {
        let root = tempfile::tempdir().unwrap();
        let rel = "etc/pki/tls/certs/ca-bundle.crt";
        let ca = root.path().join(rel);
        fs::create_dir_all(ca.parent().unwrap()).unwrap();
        fs::write(&ca, b"ca").unwrap();
        let origin = Origin::Host;
        let mut closure =
            DependencyClosure::default().add_entry(script_plan("curl", origin.clone()));
        inject_portable_data(&mut closure, &[(origin.clone(), root.path().to_path_buf())]).unwrap();
        let meta = closure.metadata.get(&origin).unwrap();
        assert!(meta
            .env_defaults
            .get("SSL_CERT_FILE")
            .unwrap()
            .ends_with("etc/pki/tls/certs/ca-bundle.crt"));
    }

    #[test]
    fn imagemagick_copies_config_and_sets_module_paths() {
        let root = tempfile::tempdir().unwrap();
        let etc = root.path().join("etc/ImageMagick-6");
        fs::create_dir_all(&etc).unwrap();
        fs::write(etc.join("delegates.xml"), b"<delegatemap/>").unwrap();
        fs::write(etc.join("policy.xml"), b"<policymap/>").unwrap();
        let coder = root
            .path()
            .join("usr/lib/x86_64-linux-gnu/ImageMagick-6.9.11/modules-Q16/coders");
        fs::create_dir_all(&coder).unwrap();
        fs::write(coder.join("png.la"), b"dlname='png.so'\n").unwrap();
        fs::write(coder.join("png.so"), b"png").unwrap();
        let filter = root
            .path()
            .join("usr/lib/x86_64-linux-gnu/ImageMagick-6.9.11/modules-Q16/filters");
        fs::create_dir_all(&filter).unwrap();
        fs::write(filter.join("analyze.la"), b"dlname='analyze.so'\n").unwrap();

        let origin = Origin::Image("im".into());
        let mut closure =
            DependencyClosure::default().add_entry(binary_plan("convert-im6.q16", origin.clone()));
        inject_portable_data(&mut closure, &[(origin.clone(), root.path().to_path_buf())]).unwrap();

        assert!(closure.files.iter().any(|file| {
            file.destination == Path::new("payload/etc/ImageMagick-6/delegates.xml")
        }));
        assert!(closure
            .files
            .iter()
            .any(|file| file.destination == Path::new("payload/etc/ImageMagick-6/policy.xml")));
        let meta = closure.metadata.get(&origin).unwrap();
        assert_eq!(
            meta.env_defaults
                .get("MAGICK_CONFIGURE_PATH")
                .map(String::as_str),
            Some("${SIDEBUNDLE_ROOT}/payload/etc/ImageMagick-6")
        );
        assert_eq!(
            meta.env_defaults
                .get("MAGICK_CODER_MODULE_PATH")
                .map(String::as_str),
            Some("${SIDEBUNDLE_ROOT}/payload/usr/lib/x86_64-linux-gnu/ImageMagick-6.9.11/modules-Q16/coders")
        );
        assert_eq!(
            meta.env_defaults
                .get("MAGICK_FILTER_MODULE_PATH")
                .map(String::as_str),
            Some("${SIDEBUNDLE_ROOT}/payload/usr/lib/x86_64-linux-gnu/ImageMagick-6.9.11/modules-Q16/filters")
        );
    }

    #[test]
    fn convert_display_name_triggers_magick_config() {
        let root = tempfile::tempdir().unwrap();
        let etc = root.path().join("etc/ImageMagick-6");
        fs::create_dir_all(&etc).unwrap();
        fs::write(etc.join("delegates.xml"), b"<delegatemap/>").unwrap();
        let origin = Origin::Image("im".into());
        let mut closure =
            DependencyClosure::default().add_entry(binary_plan("convert", origin.clone()));
        inject_portable_data(&mut closure, &[(origin.clone(), root.path().to_path_buf())]).unwrap();
        assert!(closure.files.iter().any(|file| {
            file.destination == Path::new("payload/etc/ImageMagick-6/delegates.xml")
        }));
    }
}
