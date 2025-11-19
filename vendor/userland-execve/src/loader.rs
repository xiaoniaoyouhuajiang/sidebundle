use goblin::{
    elf::{self, Elf},
    elf64::program_header::PT_LOAD,
};
use nix::{
    sys::mman::{mmap, MapFlags, ProtFlags},
    unistd::{sysconf, SysconfVar},
};
use std::{
    fs::File,
    num::NonZeroUsize,
    os::fd::BorrowedFd,
    path::{Path, PathBuf},
    ptr,
};

pub enum Interpreter {
    FromHeader,
    None,
    Path(PathBuf),
}

pub fn load(
    path: &Path,
    interpreter: Interpreter,
) -> (usize, elf::Header, Option<(usize, elf::Header)>) {
    let file = File::open(path).unwrap();
    let bytes = std::fs::read(path).unwrap();
    let elf = Elf::parse(&bytes).unwrap();
    let interp_path: Option<&Path> = match &interpreter {
        Interpreter::FromHeader => elf.interpreter.as_ref().map(|x| x.as_ref()),
        Interpreter::None => None,
        Interpreter::Path(path) => Some(path),
    };
    let opt_interp = match interp_path {
        Some(interp) => {
            let (interp_load_addr, interp_header, None) = load(interp, Interpreter::FromHeader)
            else {
                panic!()
            };
            Some((interp_load_addr, interp_header))
        }
        None => None,
    };
    let is_pie = elf
        .program_headers
        .iter()
        .find(|h| h.p_type == PT_LOAD)
        .unwrap()
        .p_vaddr
        == 0;
    assert!(is_pie);
    let total_size: usize = elf
        .program_headers
        .iter()
        .filter(|h| h.p_type == PT_LOAD)
        .map(|h| h.p_vaddr + h.p_memsz)
        .max()
        .unwrap()
        .try_into()
        .unwrap();
    let total_size = NonZeroUsize::new(total_size).unwrap();
    let base_ptr = unsafe {
        mmap::<BorrowedFd>(
            None,
            total_size,
            ProtFlags::PROT_READ | ProtFlags::PROT_WRITE, // TODO: read only fix
            MapFlags::MAP_PRIVATE | MapFlags::MAP_ANON,
            None,
            0,
        )
    }
    .unwrap();
    let base_addr = base_ptr as usize;

    let page_size: usize = sysconf(SysconfVar::PAGE_SIZE)
        .unwrap()
        .unwrap()
        .try_into()
        .unwrap();
    let page_round_down = |addr: usize| addr / page_size * page_size;
    let page_round_up = |addr: usize| (addr + (page_size - 1)) / page_size * page_size;
    for ph in elf.program_headers {
        if ph.p_type != PT_LOAD {
            continue;
        }
        assert!(ph.p_memsz >= ph.p_filesz);

        let size: usize = ph.p_filesz.try_into().unwrap();
        let prot = (ph.p_flags >> 2) | ((ph.p_flags & 0b001) << 2) | (ph.p_flags & 0b010);
        let prot = prot.try_into().unwrap();
        let prot = ProtFlags::from_bits(prot).unwrap();
        let offset: usize = ph.p_offset.try_into().unwrap();
        let vaddr: usize = ph.p_vaddr.try_into().unwrap();
        let unaligned_addr = base_addr + vaddr;
        let addr = page_round_down(unaligned_addr);
        let align_dist = unaligned_addr - addr;
        let size = size + align_dist;
        let size = NonZeroUsize::new(size).unwrap();
        let offset = offset - align_dist;
        let offset = offset.try_into().unwrap();
        let addr = NonZeroUsize::new(addr).unwrap();
        unsafe {
            mmap(
                Some(addr),
                size,
                prot | ProtFlags::PROT_WRITE, // TODO: read only fix
                MapFlags::MAP_PRIVATE | MapFlags::MAP_FIXED,
                Some(&file),
                offset,
            )
        }
        .unwrap();
        let file_end_addr = addr.get() + size.get();
        unsafe {
            ptr::write_bytes(
                file_end_addr as *mut u8,
                0,
                page_round_up(file_end_addr) - file_end_addr,
            );
        }
    }

    // Relocations (needed for musl but not glibc FWICT)
    for rel in elf.dynrelas.iter() {
        let offset: usize = rel.r_offset.try_into().unwrap();
        let addend: usize = rel.r_addend.unwrap().try_into().unwrap();
        let dst = (base_addr + offset) as *mut usize;
        let src = base_addr + addend;
        unsafe { ptr::write(dst, src) }
    }

    (base_addr, elf.header, opt_interp)
}
