use std::arch::asm;

#[cfg(target_arch = "x86_64")]
pub unsafe fn run(sp: usize, entry: usize) -> ! {
    asm! {
        "mov rsp, {sp}",
        "jmp {entry}",
        inout("rax") 0 => _,
        sp = in(reg) sp,
        entry = in(reg) entry,
    }
    unreachable!()
}

#[cfg(target_arch = "aarch64")]
pub unsafe fn run(sp: usize, entry: usize) -> ! {
    asm! {
        "mov sp, {sp}",
        "br {entry}",
        sp = in(reg) sp,
        entry = in(reg) entry,
    }
    unreachable!()
}
