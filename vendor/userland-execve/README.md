[![](https://img.shields.io/crates/v/userland-execve)](https://crates.io/crates/userland-execve)
[![](https://docs.rs/userland-execve/badge.svg)](https://docs.rs/userland-execve)

# `userland-execve`

An implementation of `execve()` in user space.

This works by mapping the ELF executable
(and interpreter, such as `ld-linux.so.2`) into memory,
creating a stack for it
(containing the auxiliary vector, arguments, and environment variables),
and then jumping to the entry point with the new stack.
