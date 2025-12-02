use super::{TraceBackend, TraceError, TraceInvocation, TraceReport};
use nix::errno::Errno;
use nix::libc;
use nix::sys::fanotify::{EventFFlags, Fanotify, InitFlags, MarkFlags, MaskFlags};
use nix::sys::ptrace;
#[cfg(target_arch = "x86_64")]
use nix::sys::ptrace::AddressType;
use nix::sys::signal::{kill, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{chdir, chroot, execve, fork, ForkResult, Pid};
use std::collections::{BTreeMap, HashMap};
use std::env;
use std::ffi::{CStr, CString, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;
#[cfg(target_arch = "x86_64")]
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

/// ptrace-based backend (legacy behavior).
#[derive(Debug, Clone, Default)]
pub struct PtraceBackend;

impl PtraceBackend {
    pub fn new() -> Self {
        Self
    }
}

impl TraceBackend for PtraceBackend {
    fn trace(&self, invocation: &TraceInvocation<'_>) -> Result<TraceReport, TraceError> {
        run_ptrace(invocation)
    }
}

/// fanotify-based backend for deep search.
#[derive(Debug, Clone)]
pub struct FanotifyBackend {
    mask: MaskFlags,
}

impl FanotifyBackend {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn mask(&self) -> MaskFlags {
        self.mask
    }
}

impl Default for FanotifyBackend {
    fn default() -> Self {
        Self {
            mask: MaskFlags::FAN_OPEN | MaskFlags::FAN_OPEN_EXEC | MaskFlags::FAN_EVENT_ON_CHILD,
        }
    }
}

impl TraceBackend for FanotifyBackend {
    fn trace(&self, invocation: &TraceInvocation<'_>) -> Result<TraceReport, TraceError> {
        run_fanotify(invocation, self.mask)
    }
}

/// Backend that merges ptrace + fanotify outputs.
#[derive(Debug, Clone, Default)]
pub struct CombinedBackend {
    ptrace: PtraceBackend,
    fanotify: FanotifyBackend,
}

impl CombinedBackend {
    pub fn new() -> Self {
        Self::default()
    }
}

impl TraceBackend for CombinedBackend {
    fn trace(&self, invocation: &TraceInvocation<'_>) -> Result<TraceReport, TraceError> {
        let mut report = self.ptrace.trace(invocation)?;
        let fan = self.fanotify.trace(invocation)?;
        report.extend(fan);
        Ok(report)
    }
}

fn run_ptrace(invocation: &TraceInvocation<'_>) -> Result<TraceReport, TraceError> {
    let argv = strings_to_cstring(invocation.command)?;
    let envp = envp_to_cstring(invocation.env)?;

    unsafe {
        match fork().map_err(TraceError::Nix)? {
            ForkResult::Child => ptrace_child_main(invocation.root, &argv, &envp),
            ForkResult::Parent { child } => parent_trace(child),
        }
    }
}

fn run_fanotify(
    invocation: &TraceInvocation<'_>,
    mask: MaskFlags,
) -> Result<TraceReport, TraceError> {
    let argv = strings_to_cstring(invocation.command)?;
    let envp = envp_to_cstring(invocation.env)?;
    let watch_root = invocation.root.unwrap_or_else(|| Path::new("/"));

    let fan = Fanotify::init(
        InitFlags::FAN_CLOEXEC | InitFlags::FAN_CLASS_NOTIF | InitFlags::FAN_NONBLOCK,
        EventFFlags::O_RDONLY | EventFFlags::O_LARGEFILE,
    )
    .map_err(|err| TraceError::Fanotify(err.to_string()))?;
    fan.mark(
        MarkFlags::FAN_MARK_ADD | MarkFlags::FAN_MARK_FILESYSTEM,
        mask,
        None,
        Some(watch_root),
    )
    .map_err(|err| TraceError::Fanotify(err.to_string()))?;

    unsafe {
        match fork().map_err(TraceError::Nix)? {
            ForkResult::Child => fanotify_child_main(invocation.root, &argv, &envp),
            ForkResult::Parent { child } => fanotify_parent(child, fan),
        }
    }
}

fn strings_to_cstring(values: &[String]) -> Result<Vec<CString>, TraceError> {
    values
        .iter()
        .map(|s| Ok(CString::new(s.as_str())?))
        .collect()
}

fn envp_to_cstring(overrides: &[(OsString, OsString)]) -> Result<Vec<CString>, TraceError> {
    let mut map: BTreeMap<OsString, OsString> = env::vars_os().collect();
    for (key, value) in overrides {
        map.insert(key.clone(), value.clone());
    }
    map.into_iter()
        .map(|(k, v)| {
            let mut bytes = Vec::new();
            bytes.extend(k.as_os_str().as_bytes());
            bytes.push(b'=');
            bytes.extend(v.as_os_str().as_bytes());
            Ok(CString::new(bytes)?)
        })
        .collect()
}

unsafe fn ptrace_child_main(root: Option<&Path>, argv: &[CString], envp: &[CString]) -> ! {
    if let Some(root) = root {
        if let Err(err) = chdir(root)
            .and_then(|_| chroot("."))
            .and_then(|_| chdir(Path::new("/")))
        {
            eprintln!("sidebundle trace: failed to chroot: {err:?}");
            std::process::exit(TraceExit::ChrootFailure as i32);
        }
    }

    if let Err(err) = ptrace::traceme() {
        eprintln!("sidebundle trace: ptrace TRACEME failed: {err:?}");
        std::process::exit(TraceExit::PtraceDenied as i32);
    }
    let _ = kill(Pid::from_raw(libc::getpid()), Signal::SIGSTOP);

    let argv_refs: Vec<&CStr> = argv.iter().map(|c| c.as_c_str()).collect();
    let envp_refs: Vec<&CStr> = envp.iter().map(|c| c.as_c_str()).collect();
    match execve(argv_refs[0], &argv_refs, &envp_refs) {
        Ok(_) => unreachable!(),
        Err(err) => {
            eprintln!("sidebundle trace: execve failed: {err:?}");
            std::process::exit(TraceExit::ExecFailure as i32);
        }
    }
}

unsafe fn fanotify_child_main(root: Option<&Path>, argv: &[CString], envp: &[CString]) -> ! {
    if let Some(root) = root {
        if let Err(err) = chdir(root)
            .and_then(|_| chroot("."))
            .and_then(|_| chdir(Path::new("/")))
        {
            eprintln!("sidebundle trace: failed to chroot: {err:?}");
            std::process::exit(TraceExit::ChrootFailure as i32);
        }
    }

    let argv_refs: Vec<&CStr> = argv.iter().map(|c| c.as_c_str()).collect();
    let envp_refs: Vec<&CStr> = envp.iter().map(|c| c.as_c_str()).collect();
    match execve(argv_refs[0], &argv_refs, &envp_refs) {
        Ok(_) => unreachable!(),
        Err(err) => {
            eprintln!("sidebundle trace: execve failed: {err:?}");
            std::process::exit(TraceExit::ExecFailure as i32);
        }
    }
}

unsafe fn parent_trace(child: Pid) -> Result<TraceReport, TraceError> {
    let mut report = TraceReport::default();
    let mut entering: HashMap<Pid, bool> = HashMap::new();

    loop {
        match waitpid(Some(child), Some(WaitPidFlag::empty())) {
            Ok(WaitStatus::Stopped(pid, Signal::SIGSTOP)) => {
                ptrace::setoptions(
                    pid,
                    ptrace::Options::PTRACE_O_TRACESYSGOOD | ptrace::Options::PTRACE_O_TRACEEXIT,
                )
                .map_err(TraceError::Nix)?;
                ptrace::syscall(pid, None).map_err(TraceError::Nix)?;
            }
            Ok(WaitStatus::PtraceSyscall(pid)) => {
                let entry = entering.entry(pid).or_insert(true);
                if *entry {
                    if let Err(err) = handle_syscall(pid, &mut report) {
                        ptrace::detach(pid, None).ok();
                        return Err(err);
                    }
                }
                *entry = !*entry;
                ptrace::syscall(pid, None).map_err(TraceError::Nix)?;
            }
            Ok(WaitStatus::PtraceEvent(pid, _, _)) => {
                ptrace::syscall(pid, None).map_err(TraceError::Nix)?;
            }
            Ok(WaitStatus::Exited(pid, status)) => {
                if pid == child {
                    return match TraceExit::from_status(status) {
                        Some(exit) => Err(map_trace_exit(exit)),
                        None => Ok(report),
                    };
                }
            }
            Ok(WaitStatus::Signaled(pid, _sig, _)) => {
                if pid == child {
                    return Err(TraceError::UnexpectedExit);
                }
            }
            Ok(WaitStatus::StillAlive) => {}
            Ok(WaitStatus::Continued(_)) => {}
            Ok(WaitStatus::Stopped(pid, _)) => {
                ptrace::syscall(pid, None).map_err(TraceError::Nix)?;
            }
            Err(err) => {
                if let nix::errno::Errno::ECHILD = err {
                    return Ok(report);
                } else {
                    return Err(TraceError::Nix(err));
                }
            }
        }
    }
}

fn fanotify_parent(child: Pid, fan: Fanotify) -> Result<TraceReport, TraceError> {
    let mut report = TraceReport::default();
    let mut child_done = false;
    let mut idle_loops: u32 = 0;

    loop {
        match fan.read_events() {
            Ok(events) => {
                if events.is_empty() {
                    if child_done {
                        idle_loops += 1;
                    }
                } else {
                    idle_loops = 0;
                    for event in events {
                        record_fanotify_event(&event, &mut report);
                    }
                }
            }
            Err(Errno::EAGAIN) => {
                if child_done {
                    idle_loops += 1;
                }
            }
            Err(err) => {
                return Err(TraceError::Fanotify(err.to_string()));
            }
        }

        if !child_done {
            match waitpid(child, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(pid, status)) if pid == child => {
                    child_done = true;
                    if let Some(exit) = TraceExit::from_status(status) {
                        return Err(map_trace_exit(exit));
                    }
                }
                Ok(WaitStatus::Signaled(pid, _sig, _)) if pid == child => {
                    return Err(TraceError::UnexpectedExit);
                }
                Ok(WaitStatus::StillAlive) | Ok(WaitStatus::Exited(_, _)) => {}
                Err(Errno::ECHILD) => {
                    child_done = true;
                }
                Err(Errno::EINTR) => {}
                Err(err) => return Err(TraceError::Nix(err)),
                _ => {}
            }
        }

        if child_done && idle_loops > 5 {
            break;
        }

        thread::sleep(Duration::from_millis(10));
    }

    Ok(report)
}

fn record_fanotify_event(event: &nix::sys::fanotify::FanotifyEvent, report: &mut TraceReport) {
    let mask = event.mask();
    if !(mask.intersects(MaskFlags::FAN_OPEN | MaskFlags::FAN_OPEN_EXEC)) {
        return;
    }
    if let Some(fd) = event.fd() {
        let proc_path = format!("/proc/self/fd/{}", fd.as_raw_fd());
        if let Ok(target) = fs::read_link(&proc_path) {
            report.record_path(target);
        }
    }
}

#[cfg(target_arch = "x86_64")]
fn handle_syscall(pid: Pid, report: &mut TraceReport) -> Result<(), TraceError> {
    let regs = ptrace::getregs(pid).map_err(TraceError::Nix)?;
    let syscall = regs.orig_rax as i64;
    let addr = match syscall {
        libc::SYS_open => regs.rdi as usize,
        libc::SYS_openat => regs.rsi as usize,
        libc::SYS_execve => regs.rdi as usize,
        libc::SYS_stat => regs.rdi as usize,
        libc::SYS_newfstatat => regs.rsi as usize,
        libc::SYS_lstat => regs.rdi as usize,
        _ => 0,
    };
    if addr == 0 {
        return Ok(());
    }
    let path = read_string(pid, addr)?;
    if !path.is_empty() {
        report.record_path(PathBuf::from(path));
    }
    Ok(())
}

#[cfg(not(target_arch = "x86_64"))]
fn handle_syscall(_pid: Pid, _report: &mut TraceReport) -> Result<(), TraceError> {
    Err(TraceError::Unsupported(
        "ptrace backend is not supported on this architecture",
    ))
}

#[cfg(target_arch = "x86_64")]
fn read_string(pid: Pid, addr: usize) -> Result<String, TraceError> {
    let mut bytes = Vec::new();
    let mut offset = 0usize;
    loop {
        let data = ptrace::read(pid, (addr + offset) as AddressType).map_err(TraceError::Nix)?;
        let data_bytes = (data as libc::c_long).to_ne_bytes();
        for byte in data_bytes {
            if byte == 0 {
                return String::from_utf8(bytes)
                    .map_err(|e| TraceError::Io(io::Error::new(io::ErrorKind::InvalidData, e)));
            }
            bytes.push(byte);
        }
        offset += data_bytes.len();
    }
}

#[repr(i32)]
enum TraceExit {
    ChrootFailure = 40,
    PtraceDenied = 41,
    ExecFailure = 42,
}

impl TraceExit {
    fn from_status(status: i32) -> Option<Self> {
        match status {
            x if x == TraceExit::ChrootFailure as i32 => Some(TraceExit::ChrootFailure),
            x if x == TraceExit::PtraceDenied as i32 => Some(TraceExit::PtraceDenied),
            x if x == TraceExit::ExecFailure as i32 => Some(TraceExit::ExecFailure),
            _ => None,
        }
    }
}

fn map_trace_exit(exit: TraceExit) -> TraceError {
    match exit {
        TraceExit::PtraceDenied => {
            TraceError::Permission("ptrace not permitted on this system".into())
        }
        TraceExit::ChrootFailure => {
            TraceError::Io(io::Error::other("failed to chroot into trace root"))
        }
        TraceExit::ExecFailure => TraceError::Io(io::Error::other("failed to exec trace command")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fanotify_backend_default_mask_includes_exec() {
        let backend = FanotifyBackend::new();
        assert!(backend.mask().contains(MaskFlags::FAN_OPEN_EXEC));
    }
}
