// SPDX-License-Identifier: GPL-2.0 WITH Linux-syscall-note
//! `ioctl()` request-number encoding, mirroring `<asm-generic/ioctl.h>`
//! and the `KESTRELFS_IOC_*` definitions in `kestrelfs_ipc.h`.
//!
//! The `libc` crate deliberately does not provide the generic `_IO`/
//! `_IOR`/`_IOW`/`_IOWR` encoding macros - they are C preprocessor
//! macros, not runtime functions, so every consumer either re-derives
//! them or depends on a crate like `nix` that already has. This module
//! re-derives them directly from the kernel's own bit-layout
//! (`Documentation/userspace-api/ioctl/ioctl-number.rst` and
//! `include/uapi/asm-generic/ioctl.h`) so the daemon has no dependency
//! beyond `libc`, and so the resulting constants can be checked
//! byte-for-byte against the values already verified from the C side
//! (see the module doc comment on the constants below).

/// Mirrors `_IOC_NRBITS`.
const NRBITS: u32 = 8;
/// Mirrors `_IOC_TYPEBITS`.
const TYPEBITS: u32 = 8;
/// Mirrors `_IOC_SIZEBITS` (generic/x86_64 default).
const SIZEBITS: u32 = 14;
/// Mirrors `_IOC_DIRBITS` (generic/x86_64 default).
#[allow(dead_code)]
const DIRBITS: u32 = 2;

const NRSHIFT: u32 = 0;
const TYPESHIFT: u32 = NRSHIFT + NRBITS;
const SIZESHIFT: u32 = TYPESHIFT + TYPEBITS;
const DIRSHIFT: u32 = SIZESHIFT + SIZEBITS;

/// Mirrors `_IOC_NONE`.
const DIR_NONE: u32 = 0;
/// Mirrors `_IOC_READ` ("userland is reading, kernel is writing" - i.e.
/// this is a `copy_to_user()` on the kernel side).
const DIR_READ: u32 = 2;

/// Mirrors the generic `_IOC(dir, type, nr, size)` macro.
const fn ioc(dir: u32, ty: u32, nr: u32, size: u32) -> libc::c_ulong {
    ((dir << DIRSHIFT) | (ty << TYPESHIFT) | (nr << NRSHIFT) | (size << SIZESHIFT)) as libc::c_ulong
}

/// Mirrors the generic `_IO(type, nr)` macro: no argument.
const fn io(ty: u32, nr: u32) -> libc::c_ulong {
    ioc(DIR_NONE, ty, nr, 0)
}

/// Mirrors the generic `_IOR(type, nr, size)` macro: kernel writes
/// `size` bytes back to userspace (a `copy_to_user()` on the kernel
/// side, i.e. userspace is *reading* the result).
const fn ior(ty: u32, nr: u32, size: u32) -> libc::c_ulong {
    ioc(DIR_READ, ty, nr, size)
}

/// Mirrors `KESTRELFS_IOC_MAGIC` (0xE0).
const MAGIC: u32 = 0xE0;

/// Mirrors `KESTRELFS_IOC_NOTIFY_RESP`.
///
/// Verified against the running kernel module during Phase 2 step 3
/// development: `0xe001` (see the exact value printed by
/// `chardev_test.c`'s output and the design chat log).
pub const NOTIFY_RESP: libc::c_ulong = io(MAGIC, 1);

/// Mirrors `KESTRELFS_IOC_GET_ABI_VERSION`.
///
/// Verified against the running kernel module: `0x8004e002` (size=4,
/// matching `__u32`).
pub const GET_ABI_VERSION: libc::c_ulong = ior(MAGIC, 2, 4);

/// Mirrors `KESTRELFS_IOC_GET_REGION_SIZE`.
///
/// Verified against the running kernel module: `0x8008e003` (size=8,
/// matching `__u64`).
pub const GET_REGION_SIZE: libc::c_ulong = ior(MAGIC, 3, 8);

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the exact numeric values already cross-checked against the
    /// live kernel module's `KESTRELFS_IOC_*` macros (see
    /// `chardev_test.c`'s verified output). If this ever fails, either
    /// this module's bit-encoding drifted from
    /// `<asm-generic/ioctl.h>`, or `kestrelfs_ipc.h`'s magic/nr/size
    /// values changed without updating this file to match.
    #[test]
    fn ioctl_numbers_match_kernel_header() {
        assert_eq!(NOTIFY_RESP, 0xe001);
        assert_eq!(GET_ABI_VERSION, 0x8004e002);
        assert_eq!(GET_REGION_SIZE, 0x8008e003);
    }
}
