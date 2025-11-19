use std::{
    ffi::{CStr, CString},
    mem::size_of,
    num::NonZeroUsize,
    os::{fd::BorrowedFd, raw::c_char},
};

use goblin::elf;
use nix::{
    libc::{
        getauxval, AT_BASE, AT_CLKTCK, AT_EGID, AT_ENTRY, AT_EUID, AT_EXECFN, AT_FLAGS, AT_GID,
        AT_HWCAP, AT_NULL, AT_PAGESZ, AT_PHDR, AT_PHENT, AT_PHNUM, AT_PLATFORM, AT_RANDOM,
        AT_SECURE, AT_UID,
    },
    sys::mman::{mmap, MapFlags, ProtFlags},
    unistd::{getegid, geteuid, getgid, getuid, SysconfVar},
};

#[derive(Clone, Default)]
pub struct AuxSnapshot {
    pub entries: Vec<(u64, u64)>,
    pub platform: Option<String>,
    pub random: Option<[u8; 16]>,
}

impl AuxSnapshot {
    pub fn new(entries: Vec<(u64, u64)>) -> Self {
        Self {
            entries,
            platform: None,
            random: None,
        }
    }

    pub fn with_platform(mut self, platform: Option<String>) -> Self {
        self.platform = platform;
        self
    }

    pub fn with_random(mut self, random: Option<[u8; 16]>) -> Self {
        self.random = random;
        self
    }

    pub fn value(&self, tag: u64) -> Option<u64> {
        self.entries
            .iter()
            .find(|(entry_tag, _)| *entry_tag == tag)
            .map(|(_, value)| *value)
    }

    pub fn platform(&self) -> Option<&str> {
        self.platform.as_deref()
    }

    pub fn random(&self) -> Option<[u8; 16]> {
        self.random
    }
}

struct StackBuilder<'a, A: AsRef<CStr>, E: AsRef<CStr>> {
    interp_addr: Option<usize>,
    bin_addr: usize,
    bin_header: elf::Header,
    stack_end_addr: usize,
    path: &'a CStr,
    args: &'a [A],
    env: &'a [E],
    stack_reversed: Vec<u8>,
    aux_snapshot: Option<&'a AuxSnapshot>,
}

impl<'a, A: AsRef<CStr>, E: AsRef<CStr>> StackBuilder<'a, A, E> {
    fn push_str(&mut self, s: impl AsRef<CStr>) -> usize {
        let s = s.as_ref();
        self.push_bytes(s.to_bytes_with_nul())
    }

    fn push_usize(&mut self, u: usize) {
        self.push_bytes(&u.to_ne_bytes());
    }

    fn push_bytes(&mut self, bytes: &[u8]) -> usize {
        for byte in bytes.iter().rev() {
            self.stack_reversed.push(*byte)
        }
        self.stack_end_addr - self.stack_reversed.len()
    }

    fn aux_value(&self, tag: u64, fallback: impl FnOnce() -> u64) -> u64 {
        self.aux_snapshot
            .and_then(|snapshot| snapshot.value(tag))
            .unwrap_or_else(fallback)
    }

    fn push_auxv(&mut self, path_addr: usize, at_platform_addr: usize, at_random_addr: usize) {
        let load_addr: u64 = self.bin_addr.try_into().unwrap();
        let path_addr = path_addr.try_into().unwrap();
        let at_platform_addr = at_platform_addr.try_into().unwrap();
        let at_random_addr = at_random_addr.try_into().unwrap();
        let at_base = self.interp_addr.unwrap_or_default().try_into().unwrap();
        let auxv = [
            (AT_NULL, 0),
            (AT_PLATFORM, at_platform_addr),
            (AT_EXECFN, path_addr),
            (
                AT_SECURE,
                self.aux_value(AT_SECURE, || unsafe { getauxval(AT_SECURE) as u64 }),
            ),
            (AT_RANDOM, at_random_addr),
            (
                AT_CLKTCK,
                self.aux_value(AT_CLKTCK, || sysconf_value(SysconfVar::CLK_TCK)),
            ),
            (
                AT_HWCAP,
                self.aux_value(AT_HWCAP, || unsafe { getauxval(AT_HWCAP) as u64 }),
            ),
            (
                AT_EGID,
                self.aux_value(AT_EGID, || getegid().as_raw().into()),
            ),
            (AT_GID, self.aux_value(AT_GID, || getgid().as_raw().into())),
            (
                AT_EUID,
                self.aux_value(AT_EUID, || geteuid().as_raw().into()),
            ),
            (AT_UID, self.aux_value(AT_UID, || getuid().as_raw().into())),
            (AT_ENTRY, load_addr + self.bin_header.e_entry),
            (AT_FLAGS, 0),
            (AT_BASE, at_base),
            (
                AT_PAGESZ,
                self.aux_value(AT_PAGESZ, || sysconf_value(SysconfVar::PAGE_SIZE)),
            ),
            (AT_PHNUM, self.bin_header.e_phnum.into()),
            (AT_PHENT, self.bin_header.e_phentsize.into()),
            (AT_PHDR, load_addr + self.bin_header.e_phoff),
        ];
        for (type_, value) in auxv {
            let type_ = type_.try_into().unwrap();
            let value = value.try_into().unwrap();
            self.push_usize(value);
            self.push_usize(type_);
        }
    }

    fn make(&mut self) {
        // Path of executable
        let path_addr = self.push_str(self.path);

        // Environment variable text
        let mut env_var_addrs = Vec::new();
        for env_var in self.env.iter().rev() {
            env_var_addrs.push(self.push_str(env_var));
        }

        // Argv text
        let mut arg_addrs = Vec::new();
        for arg in self.args.iter().rev() {
            arg_addrs.push(self.push_str(arg));
        }

        // Auxv strings
        let platform_storage;
        let at_platform_cstr = if let Some(value) = self
            .aux_snapshot
            .and_then(|snapshot| snapshot.platform())
        {
            platform_storage = CString::new(value).unwrap();
            platform_storage.as_c_str()
        } else {
            let ptr = unsafe { getauxval(AT_PLATFORM) as *const c_char };
            assert!(!ptr.is_null());
            unsafe { CStr::from_ptr(ptr) }
        };
        let at_platform_addr = self.push_str(at_platform_cstr);
        let random_bytes = if let Some(bytes) = self
            .aux_snapshot
            .and_then(|snapshot| snapshot.random())
        {
            bytes
        } else {
            let ptr = unsafe { getauxval(AT_RANDOM) as *const u8 };
            assert!(!ptr.is_null());
            let slice = unsafe { std::slice::from_raw_parts(ptr, 16) };
            let mut buf = [0u8; 16];
            buf.copy_from_slice(slice);
            buf
        };
        let at_random_addr = self.push_bytes(&random_bytes);

        // Align argc at bottom
        while (self.stack_reversed.len()
            + (arg_addrs.len() + env_var_addrs.len() + 3) * size_of::<usize>())
            % 16
            != 0
        {
            self.stack_reversed.push(0);
        }

        self.push_auxv(path_addr, at_platform_addr, at_random_addr);

        // Environment variable array
        self.push_usize(0);
        for addr in env_var_addrs {
            self.push_usize(addr);
        }

        // Argv array
        self.push_usize(0);
        for addr in arg_addrs {
            self.push_usize(addr);
        }

        // Argc
        self.push_usize(self.args.len());
    }
}

pub fn make_stack(
    interp_addr: Option<usize>,
    bin_addr: usize,
    bin_header: elf::Header,
    path: &CStr,
    args: &[impl AsRef<CStr>],
    env: &[impl AsRef<CStr>],
    aux_snapshot: Option<&AuxSnapshot>,
) -> usize {
    let stack_size = 1024 * 1024 * 10; // 10MB

    let stack = unsafe {
        mmap::<BorrowedFd>(
            None,
            NonZeroUsize::new(stack_size).unwrap(),
            ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
            MapFlags::MAP_PRIVATE
                | MapFlags::MAP_ANON
                | MapFlags::MAP_GROWSDOWN
                | MapFlags::MAP_STACK,
            None,
            0,
        )
    }
    .unwrap();

    let stack_end_addr = stack as usize + stack_size;

    let data = {
        let mut builder = StackBuilder {
            interp_addr,
            bin_addr,
            bin_header,
            stack_end_addr,
            path,
            args,
            env,
            stack_reversed: Vec::new(),
            aux_snapshot,
        };
        builder.make();
        let mut data = builder.stack_reversed;
        data.reverse();
        data
    };

    let stack_data_offset = stack_size - data.len();

    let sp = stack as usize + stack_data_offset;

    unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), sp as *mut u8, data.len()) }

    sp
}

fn sysconf_value(var: SysconfVar) -> u64 {
    nix::unistd::sysconf(var)
        .unwrap()
        .unwrap()
        .try_into()
        .unwrap()
}
