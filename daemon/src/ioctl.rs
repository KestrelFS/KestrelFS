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
/// Mirrors `_IOC_WRITE` (userspace writes an argument copied by the kernel).
const DIR_WRITE: u32 = 1;

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

/// Mirrors the generic `_IOW(type, nr, size)` macro.
const fn iow(ty: u32, nr: u32, size: u32) -> libc::c_ulong {
    ioc(DIR_WRITE, ty, nr, size)
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

/// Mirrors `KESTRELFS_IOC_INVALIDATE_CACHE_ALL`. The Redis coherence poller
/// uses this no-argument command to make every local cache entry a durable
/// miss after the shared metadata revision changes.
pub const INVALIDATE_CACHE_ALL: libc::c_ulong = io(MAGIC, 4);

/// Maximum inode ids carried by one fine-grained coherence ioctl.
pub const CACHE_INVALIDATE_INODES_MAX: usize = 64;

/// Mirrors `struct kestrelfs_cache_invalidate_inodes` exactly.
#[repr(C)]
pub struct CacheInvalidateInodes {
    pub count: u32,
    pub reserved: u32,
    pub inode_ids: [u64; CACHE_INVALIDATE_INODES_MAX],
}

impl Default for CacheInvalidateInodes {
    fn default() -> Self {
        Self {
            count: 0,
            reserved: 0,
            inode_ids: [0; CACHE_INVALIDATE_INODES_MAX],
        }
    }
}

/// Mirrors `KESTRELFS_IOC_INVALIDATE_CACHE_INODES` (`_IOW`, nr 5).
pub const INVALIDATE_CACHE_INODES: libc::c_ulong =
    iow(MAGIC, 5, std::mem::size_of::<CacheInvalidateInodes>() as u32);

const _: () = assert!(std::mem::size_of::<CacheInvalidateInodes>() == 520);
const _: () = assert!(std::mem::offset_of!(CacheInvalidateInodes, inode_ids) == 8);

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
        assert_eq!(INVALIDATE_CACHE_ALL, 0xe004);
        assert_eq!(INVALIDATE_CACHE_INODES, 0x4208e005);
        assert_eq!(std::mem::size_of::<CacheInvalidateInodes>(), 520);
        assert_eq!(std::mem::offset_of!(CacheInvalidateInodes, inode_ids), 8);
    }

    #[test]
    fn inode_invalidation_argument_encodes_at_pinned_offsets() {
        let mut request = CacheInvalidateInodes {
            count: 2,
            ..Default::default()
        };
        request.inode_ids[..2].copy_from_slice(&[0x1122_3344_5566_7788, 99]);
        // SAFETY: `request` is a live plain C-layout value and the byte slice
        // is limited to its exact size. The test only reads those bytes.
        let bytes = unsafe {
            std::slice::from_raw_parts(
                (&request as *const CacheInvalidateInodes).cast::<u8>(),
                std::mem::size_of::<CacheInvalidateInodes>(),
            )
        };
        assert_eq!(u32::from_ne_bytes(bytes[0..4].try_into().unwrap()), 2);
        assert_eq!(u32::from_ne_bytes(bytes[4..8].try_into().unwrap()), 0);
        assert_eq!(
            u64::from_ne_bytes(bytes[8..16].try_into().unwrap()),
            0x1122_3344_5566_7788
        );
        assert_eq!(u64::from_ne_bytes(bytes[16..24].try_into().unwrap()), 99);
    }
}
