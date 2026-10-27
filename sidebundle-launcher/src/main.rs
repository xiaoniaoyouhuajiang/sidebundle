use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use sidebundle_core::{RunMode, RuntimeMetadata};
use std::collections::{BTreeMap, HashSet};
use std::env;
use std::ffi::{CString, OsStr};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

/// Single-path variables whose absolute host paths should follow the payload, and whose
/// relative values or `${SIDEBUNDLE_ROOT}` tokens are resolved against the bundle root.
const PORTABLE_PATH_KEYS: &[&str] = &[
    "SSL_CERT_FILE",
    "CURL_CA_BUNDLE",
    "REQUESTS_CA_BUNDLE",
    "SSL_CERT_DIR",
    "TESSDATA_PREFIX",
    "GIT_EXEC_PATH",
    "GIT_TEMPLATE_DIR",
    "MAGICK_CONFIGURE_PATH",
    "MAGICK_CODER_MODULE_PATH",
];

const CA_REL_CANDIDATES: &[&str] = &[
    "etc/ssl/certs/ca-certificates.crt",
    "etc/ssl/cert.pem",
    "etc/pki/tls/certs/ca-bundle.crt",
    "etc/ssl/certs/ca-bundle.crt",
];

fn main() {
    if let Err(err) = run() {
        eprintln!("sidebundle launcher: {err}");
        std::process::exit(1);
    }
}

struct EnvRemapper<'a> {
    bundle_root: &'a Path,
    payload_prefix: PathBuf,
}

impl<'a> EnvRemapper<'a> {
    fn new(bundle_root: &'a Path) -> Self {
        Self {
            bundle_root,
            payload_prefix: bundle_root.join("payload"),
        }
    }

    fn remap_abs_into_payload(&self, env_map: &mut BTreeMap<String, String>, key: &str) {
        let Some(value) = env_map.get(key).cloned() else {
            return;
        };
        if !value.starts_with('/') {
            return;
        }
        let path = PathBuf::from(&value);
        if path.starts_with(&self.payload_prefix) || path.starts_with(self.bundle_root) {
            return;
        }
        let mut mapped = self.payload_prefix.clone();
        mapped.push(value.trim_start_matches('/'));
        env_map.insert(key.to_string(), mapped.to_string_lossy().into_owned());
    }

    fn remap_abs_list_into_payload(&self, env_map: &mut BTreeMap<String, String>, key: &str) {
        let Some(value) = env_map.get(key).cloned() else {
            return;
        };
        if value.is_empty() {
            return;
        }
        let remapped: Vec<String> = value
            .split(':')
            .filter(|p| !p.is_empty())
            .map(|entry| {
                if !entry.starts_with('/') {
                    return entry.to_string();
                }
                let path = PathBuf::from(entry);
                if path.starts_with(&self.payload_prefix) || path.starts_with(self.bundle_root) {
                    return entry.to_string();
                }
                let mut mapped = self.payload_prefix.clone();
                mapped.push(entry.trim_start_matches('/'));
                mapped.to_string_lossy().into_owned()
            })
            .collect();
        if !remapped.is_empty() {
            env_map.insert(key.to_string(), remapped.join(":"));
        }
    }
}

fn run() -> Result<()> {
    let exe_path = env::current_exe().context("failed to resolve launcher path")?;
    let launcher_dir = exe_path
        .parent()
        .ok_or_else(|| anyhow!("launcher missing parent directory"))?;
    let bundle_root = launcher_dir
        .parent()
        .ok_or_else(|| anyhow!("launcher missing bundle root"))?;

    let invoked = env::args_os()
        .next()
        .ok_or_else(|| anyhow!("missing argv0"))?;
    let entry_name = Path::new(&invoked)
        .file_name()
        .ok_or_else(|| anyhow!("invalid launcher invocation"))?
        .to_string_lossy()
        .into_owned();

    let config = load_config(bundle_root, &entry_name)?;
    match config {
        LauncherConfig::Binary {
            dynamic,
            binary,
            linker,
            library_paths,
            metadata,
            run_mode,
        } => {
            let payload_root = bundle_root.join("payload");
            let entry_host = bundle_root.join(&binary);
            let entry_mapped = map_bundle_path(bundle_root, &binary, run_mode);
            let _linker_mapped = linker
                .as_ref()
                .map(|rel| map_bundle_path(bundle_root, rel, run_mode));
            let argv = build_binary_argv(&entry_mapped)?;
            let env_block =
                build_env_block(bundle_root, run_mode, &library_paths, metadata.as_ref())?;
            match run_mode {
                RunMode::Host => {
                    if !dynamic {
                        exec_static(&entry_host, &argv, &env_block)?;
                        unreachable!();
                    }
                    let linker_host = linker
                        .as_ref()
                        .map(|rel| bundle_root.join(rel))
                        .ok_or_else(|| anyhow!("dynamic launcher missing linker path"))?;
                    exec_dynamic(&linker_host, &entry_host, &argv, &env_block)?;
                    unreachable!();
                }
                RunMode::Bwrap => {
                    exec_bwrap(bundle_root, &payload_root, &entry_mapped, &argv, &env_block)?;
                    unreachable!();
                }
                RunMode::Chroot => {
                    exec_chroot(bundle_root, &payload_root, &entry_mapped, &argv, &env_block)?;
                    unreachable!();
                }
            }
        }
        LauncherConfig::Script {
            dynamic,
            interpreter,
            script,
            args,
            linker,
            library_paths,
            metadata,
            run_mode,
        } => {
            let payload_root = bundle_root.join("payload");
            let interpreter_host = bundle_root.join(&interpreter);
            let interpreter_mapped = map_bundle_path(bundle_root, &interpreter, run_mode);
            let script_mapped = map_bundle_path(bundle_root, &script, run_mode);
            let _linker_mapped = linker
                .as_ref()
                .map(|rel| map_bundle_path(bundle_root, rel, run_mode));
            let argv = build_script_argv(&interpreter_mapped, &script_mapped, &args)?;
            let env_block =
                build_env_block(bundle_root, run_mode, &library_paths, metadata.as_ref())?;
            match run_mode {
                RunMode::Host => {
                    if !dynamic {
                        exec_static(&interpreter_host, &argv, &env_block)?;
                        unreachable!();
                    }
                    let linker_host = linker
                        .as_ref()
                        .map(|rel| bundle_root.join(rel))
                        .ok_or_else(|| anyhow!("dynamic launcher missing linker path"))?;
                    exec_dynamic(&linker_host, &interpreter_host, &argv, &env_block)?;
                    unreachable!();
                }
                RunMode::Bwrap => {
                    exec_bwrap(
                        bundle_root,
                        &payload_root,
                        &interpreter_mapped,
                        &argv,
                        &env_block,
                    )?;
                    unreachable!();
                }
                RunMode::Chroot => {
                    exec_chroot(
                        bundle_root,
                        &payload_root,
                        &interpreter_mapped,
                        &argv,
                        &env_block,
                    )?;
                    unreachable!();
                }
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LauncherConfig {
    Binary {
        dynamic: bool,
        binary: PathBuf,
        linker: Option<PathBuf>,
        library_paths: Vec<PathBuf>,
        metadata: Option<RuntimeMetadata>,
        #[serde(default = "default_run_mode")]
        run_mode: RunMode,
    },
    Script {
        dynamic: bool,
        interpreter: PathBuf,
        script: PathBuf,
        args: Vec<String>,
        linker: Option<PathBuf>,
        library_paths: Vec<PathBuf>,
        metadata: Option<RuntimeMetadata>,
        #[serde(default = "default_run_mode")]
        run_mode: RunMode,
    },
}

fn default_run_mode() -> RunMode {
    RunMode::Host
}

fn load_config(bundle_root: &Path, entry_name: &str) -> Result<LauncherConfig> {
    let path = bundle_root
        .join("launchers")
        .join(format!("{entry_name}.json"));
    let data = fs::read(&path)
        .with_context(|| format!("failed to read launcher config {}", path.display()))?;
    serde_json::from_slice(&data)
        .with_context(|| format!("invalid launcher config {}", path.display()))
}

fn build_binary_argv(entry: &Path) -> Result<Vec<CString>> {
    let mut argv = Vec::new();
    argv.push(os_to_cstring(entry.as_os_str())?);
    for arg in env::args_os().skip(1) {
        argv.push(os_to_cstring(&arg)?);
    }
    Ok(argv)
}

fn build_script_argv(interpreter: &Path, script: &Path, args: &[String]) -> Result<Vec<CString>> {
    let mut argv = Vec::new();
    argv.push(os_to_cstring(interpreter.as_os_str())?);
    for arg in args {
        argv.push(CString::new(arg.as_bytes()).map_err(|err| anyhow!("invalid arg: {err}"))?);
    }
    argv.push(os_to_cstring(script.as_os_str())?);
    for arg in env::args_os().skip(1) {
        argv.push(os_to_cstring(&arg)?);
    }
    Ok(argv)
}

fn build_env_block(
    bundle_root: &Path,
    run_mode: RunMode,
    library_paths: &[PathBuf],
    metadata: Option<&RuntimeMetadata>,
) -> Result<Vec<CString>> {
    let caller = env::vars().collect();
    let env_map = assemble_env(caller, metadata, bundle_root, run_mode, library_paths);
    let mut block = Vec::new();
    for (key, value) in env_map {
        let mut pair = key;
        pair.push('=');
        pair.push_str(&value);
        block.push(CString::new(pair).map_err(|err| anyhow!("invalid env: {err}"))?);
    }
    Ok(block)
}

/// Merge caller env with bundle metadata.
///
/// Precedence (highest wins): `env_overrides` (`--set-env`) > caller > `env_defaults`
/// (portable data prefixes) > `env` (agent-captured defaults). A non-empty `env` map must
/// not wipe caller variables such as `HTTPS_PROXY` or `PATH` (matrix: P0-1 / PATH_ENV).
fn assemble_env(
    caller: BTreeMap<String, String>,
    metadata: Option<&RuntimeMetadata>,
    bundle_root: &Path,
    run_mode: RunMode,
    library_paths: &[PathBuf],
) -> BTreeMap<String, String> {
    let mut protected: HashSet<String> = caller.keys().cloned().collect();
    let mut env_map = caller;
    if let Some(meta) = metadata {
        for (key, value) in &meta.env_defaults {
            env_map.entry(key.clone()).or_insert_with(|| value.clone());
        }
        for (key, value) in &meta.env {
            env_map.entry(key.clone()).or_insert_with(|| value.clone());
        }
        for (key, value) in &meta.env_overrides {
            protected.insert(key.clone());
            env_map.insert(key.clone(), value.clone());
        }
    }
    let root_text = bundle_root.to_string_lossy().into_owned();
    env_map.insert("SIDEBUNDLE_ROOT".into(), root_text.clone());
    for value in env_map.values_mut() {
        *value = expand_sidebundle_root(value, &root_text);
    }
    absolutize_portable_paths(&mut env_map, bundle_root);

    let remapper = EnvRemapper::new(bundle_root);

    remapper.remap_abs_into_payload(&mut env_map, "JAVA_HOME");

    // Map GOROOT (absolute) into bundle payload to make trimmed Go toolchains work.
    remapper.remap_abs_into_payload(&mut env_map, "GOROOT");

    if run_mode == RunMode::Host {
        remapper.remap_abs_into_payload(&mut env_map, "PYTHONHOME");
        remapper.remap_abs_list_into_payload(&mut env_map, "PYTHONPATH");
    }

    let mut mapped_path_entries: Vec<String> = Vec::new();
    if run_mode == RunMode::Host {
        if let Some(path) = env_map.get("PATH").cloned() {
            mapped_path_entries = remap_path_entries(bundle_root, run_mode, &path);
            if !mapped_path_entries.is_empty() {
                let mut combined = mapped_path_entries.join(":");
                if !path.is_empty() {
                    combined.push(':');
                    combined.push_str(&path);
                }
                env_map.insert("PATH".into(), combined);
            }
        }
    } else {
        // bwrap/chroot：确保 PATH 至少包含常见宿主目录，避免找不到 bwrap。
        let default_path = env_map
            .get("PATH")
            .cloned()
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| {
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into()
            });
        env_map.insert("PATH".into(), default_path.clone());
        mapped_path_entries = default_path
            .split(':')
            .filter(|p| !p.is_empty())
            .map(|s| s.to_string())
            .collect();
    }

    if !library_paths.is_empty() {
        let mut entries: Vec<String> = library_paths
            .iter()
            .map(|path| map_bundle_path(bundle_root, path, run_mode))
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        if let Some(java_home) = env_map.get("JAVA_HOME") {
            let jh = map_env_path(java_home, run_mode);
            entries.push(jh.join("lib").to_string_lossy().into_owned());
            entries.push(jh.join("lib/server").to_string_lossy().into_owned());
        }
        // Heuristic: for each PATH bin entry, also add sibling lib directories to help JVM loads.
        for bin in &mapped_path_entries {
            let bin_path = Path::new(bin);
            if let Some(parent) = bin_path.parent() {
                entries.push(parent.join("lib").to_string_lossy().into_owned());
                entries.push(parent.join("lib/server").to_string_lossy().into_owned());
            }
        }
        dedup_strings(&mut entries);
        let joined_new = entries.join(":");
        if let Some(existing) = env_map.get("LD_LIBRARY_PATH").cloned() {
            let mut combined = String::new();
            combined.push_str(&joined_new);
            if !existing.is_empty() {
                if !combined.is_empty() {
                    combined.push(':');
                }
                combined.push_str(&existing);
            }
            env_map.insert("LD_LIBRARY_PATH".into(), combined);
        } else {
            env_map.insert("LD_LIBRARY_PATH".into(), joined_new);
        }
    }

    for key in PORTABLE_PATH_KEYS {
        remap_portable_if_present(&mut env_map, bundle_root, key);
    }
    repair_portable_prefixes(&mut env_map, bundle_root, &protected);
    env_map
}

fn absolutize_portable_paths(env_map: &mut BTreeMap<String, String>, bundle_root: &Path) {
    for key in PORTABLE_PATH_KEYS {
        let Some(value) = env_map.get(*key).cloned() else {
            continue;
        };
        if value.is_empty() || value.starts_with('/') {
            continue;
        }
        let joined = bundle_root.join(&value);
        env_map.insert((*key).to_string(), joined.to_string_lossy().into_owned());
    }
}

fn remap_portable_if_present(
    env_map: &mut BTreeMap<String, String>,
    bundle_root: &Path,
    key: &str,
) {
    let Some(value) = env_map.get(key).cloned() else {
        return;
    };
    if !value.starts_with('/') {
        return;
    }
    let payload = bundle_root.join("payload");
    let path = PathBuf::from(&value);
    if path.starts_with(&payload) || path.starts_with(bundle_root) {
        return;
    }
    let mut mapped = payload;
    mapped.push(value.trim_start_matches('/'));
    if mapped.exists() {
        env_map.insert(key.to_string(), mapped.to_string_lossy().into_owned());
    }
}

fn repair_portable_prefixes(
    env_map: &mut BTreeMap<String, String>,
    bundle_root: &Path,
    protected: &HashSet<String>,
) {
    let payload = bundle_root.join("payload");
    if !protected.contains("SSL_CERT_FILE") && !configured_path_exists(env_map.get("SSL_CERT_FILE"))
    {
        if let Some(ca) = find_ca_file(&payload) {
            let text = ca.to_string_lossy().into_owned();
            env_map.insert("SSL_CERT_FILE".into(), text.clone());
            if !protected.contains("CURL_CA_BUNDLE")
                && !configured_path_exists(env_map.get("CURL_CA_BUNDLE"))
            {
                env_map.insert("CURL_CA_BUNDLE".into(), text.clone());
            }
            if !protected.contains("REQUESTS_CA_BUNDLE")
                && !configured_path_exists(env_map.get("REQUESTS_CA_BUNDLE"))
            {
                env_map.insert("REQUESTS_CA_BUNDLE".into(), text);
            }
        }
    }
    if !protected.contains("CURL_CA_BUNDLE")
        && !configured_path_exists(env_map.get("CURL_CA_BUNDLE"))
    {
        if let Some(value) = env_map.get("SSL_CERT_FILE").cloned() {
            if Path::new(&value).exists() {
                env_map.insert("CURL_CA_BUNDLE".into(), value);
            }
        }
    }
    if !protected.contains("TESSDATA_PREFIX")
        && !configured_path_exists(env_map.get("TESSDATA_PREFIX"))
    {
        if let Some(dir) = find_tessdata_dir(&payload) {
            env_map.insert("TESSDATA_PREFIX".into(), dir.to_string_lossy().into_owned());
        }
    }
}

fn configured_path_exists(value: Option<&String>) -> bool {
    value.is_some_and(|text| !text.is_empty() && Path::new(text).exists())
}

fn find_ca_file(payload: &Path) -> Option<PathBuf> {
    CA_REL_CANDIDATES
        .iter()
        .map(|rel| payload.join(rel))
        .find(|path| path.is_file())
}

fn find_tessdata_dir(payload: &Path) -> Option<PathBuf> {
    let mut candidates = vec![
        payload.join("usr/share/tessdata"),
        payload.join("usr/local/share/tessdata"),
        payload.join("usr/share/tesseract-ocr/tessdata"),
    ];
    let version_root = payload.join("usr/share/tesseract-ocr");
    if let Ok(entries) = fs::read_dir(&version_root) {
        for entry in entries.flatten() {
            candidates.push(entry.path().join("tessdata"));
            candidates.push(entry.path());
        }
    }
    let mut fallback = None;
    for dir in candidates {
        if !dir.is_dir() {
            continue;
        }
        if dir.join("eng.traineddata").is_file() {
            return Some(dir);
        }
        if fallback.is_none() && dir_has_traineddata(&dir) {
            fallback = Some(dir);
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

fn expand_sidebundle_root(value: &str, root: &str) -> String {
    let with_braces = value.replace("${SIDEBUNDLE_ROOT}", root);
    let needle = "$SIDEBUNDLE_ROOT";
    let mut out = String::new();
    let mut rest = with_braces.as_str();
    while let Some(idx) = rest.find(needle) {
        out.push_str(&rest[..idx]);
        let after = &rest[idx + needle.len()..];
        let glued = after
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_');
        if glued {
            out.push_str(needle);
        } else {
            out.push_str(root);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

fn remap_path_entries(bundle_root: &Path, mode: RunMode, path: &str) -> Vec<String> {
    path.split(':')
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .map(|p| {
            map_bundle_path(bundle_root, &p, mode)
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

fn dedup_strings(values: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    values.retain(|v| seen.insert(v.clone()));
}

fn map_bundle_path(bundle_root: &Path, rel: &Path, mode: RunMode) -> PathBuf {
    match mode {
        RunMode::Host => {
            if rel.is_absolute() {
                // For host mode we still want the traced absolute paths to resolve to the
                // bundled payload rather than the host filesystem.
                let stripped = rel.strip_prefix("/").unwrap_or(rel);
                bundle_root.join(stripped)
            } else {
                bundle_root.join(rel)
            }
        }
        RunMode::Bwrap | RunMode::Chroot => {
            if rel.is_absolute() {
                return rel.to_path_buf();
            }
            let mut comps = rel.components();
            if let Some(first) = comps.next() {
                if first.as_os_str() == "payload" {
                    let mut rebuilt = PathBuf::from("/");
                    for c in comps {
                        rebuilt.push(c.as_os_str());
                    }
                    return rebuilt;
                }
            }
            let mut rebuilt = PathBuf::from("/");
            rebuilt.push(rel);
            rebuilt
        }
    }
}

fn ensure_payload_data(bundle_root: &Path, payload_root: &Path) -> Result<()> {
    if let Err(err) = isolate_mount_namespace() {
        eprintln!("sidebundle launcher: warning: failed to isolate mount namespace ({err})");
    }

    let source = bundle_root.join("data");
    if !source.exists() {
        return Ok(());
    }
    let dest = payload_root.join("data");
    if dest.exists() {
        // If it already has contents, assume a previous run populated it.
        if dest
            .read_dir()
            .map(|mut it| it.next().is_some())
            .unwrap_or(false)
        {
            return Ok(());
        }
    }

    fs::create_dir_all(&dest)
        .with_context(|| format!("failed to prepare chroot data dir {}", dest.display()))?;

    // Prefer a bind mount to avoid duplicating data; fall back to hardlink/copy if not permitted.
    let source_c = os_to_cstring(source.as_os_str())?;
    let dest_c = os_to_cstring(dest.as_os_str())?;
    unsafe {
        if libc::mount(
            source_c.as_ptr(),
            dest_c.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND | libc::MS_REC,
            std::ptr::null(),
        ) == 0
        {
            return Ok(());
        }
    }
    let mount_err = std::io::Error::last_os_error();
    mirror_data_tree(&source, &dest).with_context(|| {
        format!("failed to mirror data into chroot (bind mount failed: {mount_err})")
    })
}

fn mirror_data_tree(source: &Path, dest: &Path) -> Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dest.join(entry.file_name());
        let ftype = entry.file_type()?;
        if ftype.is_dir() {
            fs::create_dir_all(&dst_path)?;
            mirror_data_tree(&src_path, &dst_path)?;
        } else if ftype.is_symlink() {
            let target = fs::read_link(&src_path)?;
            if dst_path.exists() {
                let _ = fs::remove_file(&dst_path);
            }
            symlink(target, &dst_path)?;
        } else {
            // Try hardlink first to save space; fall back to copy.
            match fs::hard_link(&src_path, &dst_path) {
                Ok(_) => {}
                Err(_) => {
                    fs::copy(&src_path, &dst_path)?;
                }
            }
        }
    }
    Ok(())
}

/// Best-effort: isolate into a fresh mount namespace and make mounts private so bind mounts
/// (payload/data) don't leak back to the host namespace.
fn isolate_mount_namespace() -> Result<()> {
    unsafe {
        if libc::unshare(libc::CLONE_NEWNS) != 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| "failed to unshare mount namespace");
        }
        let root = CString::new("/")?;
        if libc::mount(
            std::ptr::null(),
            root.as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        ) != 0
        {
            return Err(io::Error::last_os_error())
                .with_context(|| "failed to set mount propagation to private");
        }
    }
    Ok(())
}

fn map_env_path(value: &str, mode: RunMode) -> PathBuf {
    let p = Path::new(value);
    if !p.is_absolute() {
        return p.to_path_buf();
    }
    match mode {
        RunMode::Host => p.to_path_buf(),
        RunMode::Bwrap | RunMode::Chroot => Path::new("/").join(p.strip_prefix("/").unwrap_or(p)),
    }
}

fn find_bwrap() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("SIDEBUNDLE_BWRAP") {
        let candidate = PathBuf::from(path);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':').filter(|p| !p.is_empty()) {
            let candidate = PathBuf::from(dir).join("bwrap");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    for candidate in ["/usr/bin/bwrap", "/usr/local/bin/bwrap", "/bin/bwrap"] {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return Some(path);
        }
    }

    #[cfg(feature = "embedded-bwrap")]
    if let Ok(path) = ensure_embedded_bwrap() {
        return Some(path);
    }
    None
}

#[cfg(feature = "embedded-bwrap")]
const EMBEDDED_BWRAP_BYTES: &[u8] = include_bytes!(env!("SIDEBUNDLE_EMBED_BWRAP_BIN_PATH"));

#[cfg(feature = "embedded-bwrap")]
fn embedded_bwrap_is_placeholder() -> bool {
    env!("SIDEBUNDLE_EMBED_BWRAP_IS_PLACEHOLDER") == "1"
}

#[cfg(feature = "embedded-bwrap")]
fn embedded_bwrap_id() -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(EMBEDDED_BWRAP_BYTES);
    format!("{digest:x}")
}

#[cfg(feature = "embedded-bwrap")]
fn embedded_bwrap_cache_path() -> Result<PathBuf> {
    let cache_root = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .ok_or_else(|| anyhow!("failed to resolve cache dir (XDG_CACHE_HOME or HOME)"))?;

    let arch = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "unknown"
    };
    Ok(cache_root
        .join("sidebundle")
        .join("bwrap")
        .join(arch)
        .join(embedded_bwrap_id())
        .join("bwrap"))
}

#[cfg(feature = "embedded-bwrap")]
fn ensure_embedded_bwrap() -> Result<PathBuf> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::PermissionsExt;

    if embedded_bwrap_is_placeholder() {
        return Err(anyhow!(
            "embedded bwrap not configured (missing SIDEBUNDLE_EMBED_BWRAP_BIN at build time)"
        ));
    }

    let dest = embedded_bwrap_cache_path()?;
    if dest.is_file() {
        return Ok(dest);
    }
    let Some(parent) = dest.parent() else {
        return Err(anyhow!("invalid embedded bwrap cache path"));
    };
    fs::create_dir_all(parent).map_err(|e| anyhow!("failed to create cache dir: {e}"))?;

    let lock_path = parent.join(".extract.lock");
    let lock_file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("failed to open lock file {}", lock_path.display()))?;
    unsafe {
        if libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX) != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| "flock failed for embedded bwrap extraction");
        }
    }

    if dest.is_file() {
        return Ok(dest);
    }

    let tmp = parent.join(format!(
        ".bwrap.tmp.{}.{}",
        std::process::id(),
        embedded_bwrap_id()
    ));
    fs::write(&tmp, EMBEDDED_BWRAP_BYTES)
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("failed to chmod {}", tmp.display()))?;
    fs::rename(&tmp, &dest)
        .with_context(|| format!("failed to rename {} -> {}", tmp.display(), dest.display()))?;

    Ok(dest)
}

fn os_to_cstring(value: &OsStr) -> Result<CString> {
    CString::new(value.as_bytes()).map_err(|err| anyhow!("invalid string: {err}"))
}

fn exec_static(entry: &Path, argv: &[CString], envp: &[CString]) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    use std::ptr;

    let entry_cstr =
        CString::new(entry.as_os_str().as_bytes()).map_err(|err| anyhow!("invalid path: {err}"))?;
    let mut argv_ptrs: Vec<*const libc::c_char> = argv.iter().map(|arg| arg.as_ptr()).collect();
    argv_ptrs.push(ptr::null());
    let mut env_ptrs: Vec<*const libc::c_char> = envp.iter().map(|env| env.as_ptr()).collect();
    env_ptrs.push(ptr::null());

    unsafe {
        libc::execve(entry_cstr.as_ptr(), argv_ptrs.as_ptr(), env_ptrs.as_ptr());
    }
    Err(std::io::Error::last_os_error())
        .with_context(|| format!("execve failed for {}", entry.display()))
}

fn exec_dynamic(linker: &Path, entry: &Path, argv: &[CString], envp: &[CString]) -> Result<()> {
    use std::ptr;

    let linker_cstr = os_to_cstring(linker.as_os_str())?;
    let entry_cstr = os_to_cstring(entry.as_os_str())?;

    let mut argv_ptrs: Vec<*const libc::c_char> = Vec::with_capacity(argv.len() + 2);
    // Keep argv[0] as the entry to satisfy multi-call binaries that validate argv0.
    argv_ptrs.push(entry_cstr.as_ptr());
    argv_ptrs.push(entry_cstr.as_ptr());
    for arg in argv.iter().skip(1) {
        argv_ptrs.push(arg.as_ptr());
    }
    argv_ptrs.push(ptr::null());

    let mut env_ptrs: Vec<*const libc::c_char> = envp.iter().map(|env| env.as_ptr()).collect();
    env_ptrs.push(ptr::null());

    unsafe {
        libc::execve(linker_cstr.as_ptr(), argv_ptrs.as_ptr(), env_ptrs.as_ptr());
    }
    Err(std::io::Error::last_os_error()).with_context(|| {
        format!(
            "execve failed for linker {} (entry {})",
            linker.display(),
            entry.display()
        )
    })
}

fn exec_bwrap(
    bundle_root: &Path,
    payload_root: &Path,
    entry: &Path,
    argv: &[CString],
    envp: &[CString],
) -> Result<()> {
    let bwrap_bin = find_bwrap()
        .context("bubblewrap (bwrap) not found in PATH; required for run_mode=bwrap")?;

    let mut args: Vec<CString> = vec![
        os_to_cstring(bwrap_bin.as_os_str())?,
        CString::new("--bind")?,
        CString::new(payload_root.to_string_lossy().to_string())?,
        CString::new("/")?,
    ];
    let data_root = bundle_root.join("data");
    if data_root.exists() {
        // ensure target mountpoint exists under new root (/data)
        let _ = std::fs::create_dir_all(payload_root.join("data"));
        args.push(CString::new("--bind")?);
        args.push(CString::new(data_root.to_string_lossy().to_string())?);
        args.push(CString::new("/data")?);
    }
    args.push(CString::new("--proc")?);
    args.push(CString::new("/proc")?);
    // minimal device/tempo mounts
    args.push(CString::new("--dev-bind")?);
    args.push(CString::new("/dev/null")?);
    args.push(CString::new("/dev/null")?);
    args.push(CString::new("--dev-bind")?);
    args.push(CString::new("/dev/zero")?);
    args.push(CString::new("/dev/zero")?);
    args.push(CString::new("--dev-bind")?);
    args.push(CString::new("/dev/tty")?);
    args.push(CString::new("/dev/tty")?);
    args.push(CString::new("--dev-bind")?);
    args.push(CString::new("/dev/urandom")?);
    args.push(CString::new("/dev/urandom")?);
    args.push(CString::new("--tmpfs")?);
    args.push(CString::new("/tmp")?);
    args.push(CString::new("--tmpfs")?);
    args.push(CString::new("/run")?);
    args.push(CString::new("--tmpfs")?);
    args.push(CString::new("/dev/shm")?);
    // DNS/hosts
    for file in ["/etc/resolv.conf", "/etc/hosts"] {
        args.push(CString::new("--ro-bind")?);
        args.push(CString::new(file)?);
        args.push(CString::new(file)?);
    }
    args.push(CString::new("--die-with-parent")?);
    args.push(CString::new("--unshare-all")?);
    args.push(CString::new("--")?);

    // For bwrap/chroot we rely on the kernel to use the bundled PT_INTERP inside the sandbox.
    args.push(os_to_cstring(entry.as_os_str())?);
    for arg in argv.iter().skip(1) {
        args.push(arg.clone());
    }

    let mut argv_ptrs: Vec<*const libc::c_char> = args.iter().map(|c| c.as_ptr()).collect();
    argv_ptrs.push(std::ptr::null());
    let mut env_ptrs: Vec<*const libc::c_char> = envp.iter().map(|e| e.as_ptr()).collect();
    env_ptrs.push(std::ptr::null());

    unsafe {
        libc::execve(args[0].as_ptr(), argv_ptrs.as_ptr(), env_ptrs.as_ptr());
    }
    Err(std::io::Error::last_os_error()).with_context(|| "execve failed for bubblewrap launcher")
}

fn exec_chroot(
    bundle_root: &Path,
    payload_root: &Path,
    entry: &Path,
    argv: &[CString],
    envp: &[CString],
) -> Result<()> {
    ensure_payload_data(bundle_root, payload_root)?;
    unsafe {
        let root_c = os_to_cstring(payload_root.as_os_str())?;
        if libc::chroot(root_c.as_ptr()) != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| "chroot failed for launcher");
        }
        if libc::chdir(c"/".as_ptr() as *const libc::c_char) != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| "chdir after chroot failed");
        }
    }
    // Once chrooted, the PT_INTERP inside the payload points to bundled ld-linux; exec the entry
    // directly so /proc/self/exe matches the intended binary (important for multi-call binaries).
    exec_static(entry, argv, envp)
}

#[cfg(test)]
mod tests {
    use super::{assemble_env, expand_sidebundle_root, EnvRemapper, RunMode};
    use sidebundle_core::RuntimeMetadata;
    use std::collections::BTreeMap;
    use std::path::Path;

    #[test]
    fn find_bwrap_uses_explicit_override() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("bwrap");
        std::fs::write(&fake, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        unsafe {
            std::env::set_var("SIDEBUNDLE_BWRAP", &fake);
            std::env::set_var("PATH", "");
        }

        let found = super::find_bwrap().unwrap();
        assert_eq!(found, fake);

        unsafe {
            std::env::remove_var("SIDEBUNDLE_BWRAP");
        }
    }

    #[test]
    fn remap_abs_into_payload_is_idempotent() {
        let bundle_root = Path::new("/tmp/bundle");
        let remapper = EnvRemapper::new(bundle_root);
        let mut env_map: BTreeMap<String, String> = BTreeMap::new();

        env_map.insert("JAVA_HOME".into(), "/usr/lib/jvm/java".into());
        remapper.remap_abs_into_payload(&mut env_map, "JAVA_HOME");
        assert_eq!(
            env_map.get("JAVA_HOME").unwrap(),
            "/tmp/bundle/payload/usr/lib/jvm/java"
        );

        remapper.remap_abs_into_payload(&mut env_map, "JAVA_HOME");
        assert_eq!(
            env_map.get("JAVA_HOME").unwrap(),
            "/tmp/bundle/payload/usr/lib/jvm/java"
        );
    }

    #[test]
    fn remap_abs_list_into_payload_keeps_relative_entries() {
        let bundle_root = Path::new("/tmp/bundle");
        let remapper = EnvRemapper::new(bundle_root);
        let mut env_map: BTreeMap<String, String> = BTreeMap::new();
        env_map.insert("PYTHONPATH".into(), "rel:/usr/lib/python3.10:/x".into());

        // Mimic host-only behavior at call site; the method itself does not check RunMode.
        let _mode = RunMode::Host;
        remapper.remap_abs_list_into_payload(&mut env_map, "PYTHONPATH");

        assert_eq!(
            env_map.get("PYTHONPATH").unwrap(),
            "rel:/tmp/bundle/payload/usr/lib/python3.10:/tmp/bundle/payload/x"
        );
    }

    fn caller(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn metadata_env_does_not_wipe_caller() {
        let mut meta = RuntimeMetadata::default();
        meta.env.insert("PATH".into(), "/usr/bin".into());
        meta.env.insert("HOME".into(), "/root".into());
        meta.env.insert("FOO".into(), "from-agent".into());
        let merged = assemble_env(
            caller(&[
                ("PATH", "/caller/bin"),
                ("HOME", "/home/user"),
                ("HTTPS_PROXY", "http://127.0.0.1:9"),
                ("BAR", "2"),
            ]),
            Some(&meta),
            Path::new("/tmp/bundle"),
            RunMode::Host,
            &[],
        );
        assert_eq!(
            merged.get("PATH").unwrap().split(':').next().unwrap(),
            "/tmp/bundle/caller/bin"
        );
        assert!(merged.get("PATH").unwrap().contains("/caller/bin"));
        assert_eq!(merged.get("HOME").unwrap(), "/home/user");
        assert_eq!(merged.get("HTTPS_PROXY").unwrap(), "http://127.0.0.1:9");
        assert_eq!(merged.get("BAR").unwrap(), "2");
        assert_eq!(merged.get("FOO").unwrap(), "from-agent");
    }

    #[test]
    fn set_env_overrides_caller_and_agent() {
        let mut meta = RuntimeMetadata::default();
        meta.env.insert(
            "SSL_CERT_FILE".into(),
            "/etc/ssl/certs/ca-certificates.crt".into(),
        );
        meta.env_overrides
            .insert("SSL_CERT_FILE".into(), "/custom/ca.crt".into());
        let merged = assemble_env(
            caller(&[
                ("SSL_CERT_FILE", "/caller/ca.crt"),
                ("HTTPS_PROXY", "http://proxy"),
            ]),
            Some(&meta),
            Path::new("/tmp/bundle"),
            RunMode::Host,
            &[],
        );
        assert_eq!(merged.get("SSL_CERT_FILE").unwrap(), "/custom/ca.crt");
        assert_eq!(merged.get("HTTPS_PROXY").unwrap(), "http://proxy");
    }

    #[test]
    fn expands_sidebundle_root_and_relative_payload_paths() {
        assert_eq!(
            expand_sidebundle_root("${SIDEBUNDLE_ROOT}/payload/ca.crt", "/opt/bundle"),
            "/opt/bundle/payload/ca.crt"
        );
        assert_eq!(
            expand_sidebundle_root("$SIDEBUNDLE_ROOT/payload/ca.crt", "/opt/bundle"),
            "/opt/bundle/payload/ca.crt"
        );
        assert_eq!(
            expand_sidebundle_root("$SIDEBUNDLE_ROOT_FOO", "/opt/bundle"),
            "$SIDEBUNDLE_ROOT_FOO"
        );

        let mut meta = RuntimeMetadata::default();
        meta.env_overrides.insert(
            "SSL_CERT_FILE".into(),
            "${SIDEBUNDLE_ROOT}/payload/etc/ssl/certs/ca-certificates.crt".into(),
        );
        meta.env_defaults.insert(
            "TESSDATA_PREFIX".into(),
            "payload/usr/share/tessdata".into(),
        );
        let merged = assemble_env(
            caller(&[]),
            Some(&meta),
            Path::new("/opt/bundle"),
            RunMode::Host,
            &[],
        );
        assert_eq!(
            merged.get("SSL_CERT_FILE").unwrap(),
            "/opt/bundle/payload/etc/ssl/certs/ca-certificates.crt"
        );
        assert_eq!(
            merged.get("TESSDATA_PREFIX").unwrap(),
            "/opt/bundle/payload/usr/share/tessdata"
        );
    }

    #[test]
    fn caller_prefix_beats_agent_and_product_default() {
        let mut meta = RuntimeMetadata::default();
        meta.env
            .insert("TESSDATA_PREFIX".into(), "/usr/share/tessdata".into());
        meta.env_defaults.insert(
            "TESSDATA_PREFIX".into(),
            "/opt/bundle/payload/tessdata".into(),
        );
        let merged = assemble_env(
            caller(&[("TESSDATA_PREFIX", "/custom/tessdata")]),
            Some(&meta),
            Path::new("/opt/bundle"),
            RunMode::Host,
            &[],
        );
        assert_eq!(merged.get("TESSDATA_PREFIX").unwrap(), "/custom/tessdata");
    }

    #[test]
    fn product_default_beats_agent_env() {
        let mut meta = RuntimeMetadata::default();
        meta.env.insert(
            "SSL_CERT_FILE".into(),
            "/etc/ssl/certs/ca-certificates.crt".into(),
        );
        meta.env_defaults
            .insert("SSL_CERT_FILE".into(), "/opt/bundle/payload/ca.crt".into());
        let merged = assemble_env(
            caller(&[("HTTPS_PROXY", "http://proxy")]),
            Some(&meta),
            Path::new("/opt/bundle"),
            RunMode::Host,
            &[],
        );
        assert_eq!(
            merged.get("SSL_CERT_FILE").unwrap(),
            "/opt/bundle/payload/ca.crt"
        );
        assert_eq!(merged.get("HTTPS_PROXY").unwrap(), "http://proxy");
    }

    #[test]
    fn repairs_missing_ca_and_tessdata_from_payload() {
        let root = tempfile::tempdir().unwrap();
        let ca = root
            .path()
            .join("payload/etc/ssl/certs/ca-certificates.crt");
        std::fs::create_dir_all(ca.parent().unwrap()).unwrap();
        std::fs::write(&ca, b"ca").unwrap();
        let tess = root.path().join("payload/usr/share/tessdata");
        std::fs::create_dir_all(&tess).unwrap();
        std::fs::write(tess.join("eng.traineddata"), b"eng").unwrap();

        let mut meta = RuntimeMetadata::default();
        meta.env.insert(
            "SSL_CERT_FILE".into(),
            "/etc/ssl/certs/ca-certificates.crt".into(),
        );
        meta.env
            .insert("TESSDATA_PREFIX".into(), "/usr/share/tessdata".into());
        let merged = assemble_env(
            caller(&[("LANG", "C")]),
            Some(&meta),
            root.path(),
            RunMode::Host,
            &[],
        );
        assert_eq!(
            merged.get("SSL_CERT_FILE").map(String::as_str),
            Some(ca.to_str().unwrap())
        );
        assert_eq!(
            merged.get("CURL_CA_BUNDLE").map(String::as_str),
            Some(ca.to_str().unwrap())
        );
        assert_eq!(
            merged.get("TESSDATA_PREFIX").map(String::as_str),
            Some(tess.to_str().unwrap())
        );
        assert_eq!(merged.get("LANG").unwrap(), "C");
    }

    #[test]
    fn does_not_replace_caller_ca_or_invent_missing_payload() {
        let merged = assemble_env(
            caller(&[("SSL_CERT_FILE", "/etc/ssl/certs/ca-certificates.crt")]),
            None,
            Path::new("/tmp/bundle-does-not-exist"),
            RunMode::Host,
            &[],
        );
        assert_eq!(
            merged.get("SSL_CERT_FILE").unwrap(),
            "/etc/ssl/certs/ca-certificates.crt"
        );
        assert!(!merged.contains_key("TESSDATA_PREFIX"));
    }
}
