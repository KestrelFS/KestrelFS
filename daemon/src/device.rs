// SPDX-License-Identifier: GPL-2.0 WITH Linux-syscall-note
//! `/dev/kestrel_ctl` device handle: open, ABI/size validation, and
//! `mmap()` of the shared ring-buffer region.
//!
//! This module owns the only two raw OS resources the daemon holds: the
//! device file descriptor and the `mmap()`ed region. Both are released
//! deterministically via [`Drop`], so a `KestrelDevice` going out of
//! scope always leaves no dangling mapping or leaked fd behind - the
//! same "no leaks even on the unhappy path" property the kernel side's
//! `chardev.c` enforces via its own `misc_deregister()`/`vfree()`
//! ordering (see that file's `kestrelfs_chardev_exit()` comment).

use crate::abi::{KestrelfsSharedRegion, ABI_VERSION, SHM_REGION_SIZE};
use crate::ioctl;
use std::ffi::CStr;
use std::io;
use std::os::unix::io::RawFd;

/// Path to the KestrelFS IPC bridge char device, registered by
/// `chardev.c`'s `kestrelfs_chardev_init()` at module load time.
const DEVICE_PATH: &CStr = c"/dev/kestrel_ctl";

/// An open handle to `/dev/kestrel_ctl` with its shared ring-buffer
/// region mapped into this process' address space.
///
/// # Invariants
///
/// - `fd` is always a valid, open file descriptor for the lifetime of
///   this struct (opened in [`KestrelDevice::open`], closed in
///   [`Drop::drop`]).
/// - `region` always points at a live `mmap()` mapping of exactly
///   `SHM_REGION_SIZE` bytes, backed by that same `fd`, for the
///   lifetime of this struct.
pub struct KestrelDevice {
    fd: RawFd,
    region: *mut KestrelfsSharedRegion,
}

impl KestrelDevice {
    /// Opens `/dev/kestrel_ctl`, validates the kernel module's
    /// reported ABI version and shared-region size against what this
    /// binary was built against, then `mmap()`s the region.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if the device cannot be opened, either
    /// `ioctl()` call fails, the reported ABI version or region size
    /// does not match this build's [`ABI_VERSION`]/[`SHM_REGION_SIZE`]
    /// (surfaced as [`io::ErrorKind::InvalidData`] - deliberately
    /// refusing to proceed rather than risk misinterpreting a
    /// differently-laid-out region), or `mmap()` fails.
    pub fn open() -> io::Result<Self> {
        // SAFETY: `DEVICE_PATH` is a valid, NUL-terminated C string
        // (guaranteed by the `c"..."` literal) that outlives this
        // call. `O_RDWR` requests read/write access, matching that we
        // both read requests and write responses into the mapped
        // region.
        let fd = unsafe { libc::open(DEVICE_PATH.as_ptr(), libc::O_RDWR) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        // From this point on, any early-return error path must close
        // `fd` before propagating, since we have not yet constructed
        // a `KestrelDevice` (whose `Drop` would otherwise do it for
        // us). `close_and_return` centralizes that.
        match Self::open_inner(fd) {
            Ok(dev) => Ok(dev),
            Err(e) => {
                // SAFETY: `fd` was just returned by a successful
                // `open()` above and has not been closed yet.
                unsafe {
                    libc::close(fd);
                }
                Err(e)
            }
        }
    }

    /// Continuation of [`Self::open`] once we have a valid `fd`,
    /// factored out so every error path in the ABI-check / mmap
    /// sequence can share one "close `fd` on failure" wrapper at the
    /// call site instead of duplicating cleanup in each branch.
    fn open_inner(fd: RawFd) -> io::Result<Self> {
        let kernel_abi_version = Self::ioctl_get_abi_version(fd)?;
        if kernel_abi_version != ABI_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "kestrelfs ABI mismatch: kernel module reports version {kernel_abi_version}, \
                     this daemon was built against version {ABI_VERSION}"
                ),
            ));
        }

        let kernel_region_size = Self::ioctl_get_region_size(fd)?;
        if kernel_region_size != SHM_REGION_SIZE as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "kestrelfs shared region size mismatch: kernel module reports {kernel_region_size} \
                     bytes, this daemon was built expecting {SHM_REGION_SIZE} bytes"
                ),
            ));
        }

        // SAFETY: `fd` is a valid, open file descriptor for
        // `/dev/kestrel_ctl`, whose `.mmap` handler
        // (`kestrelfs_mmap()` in `chardev.c`) we have just confirmed,
        // via the two ioctl checks above, agrees with this process on
        // both the ABI version and the exact region size. We request
        // exactly `SHM_REGION_SIZE` bytes at offset 0, matching what
        // `kestrelfs_mmap()` requires (it rejects any other length or
        // a nonzero `pgoff`). `PROT_READ | PROT_WRITE` matches that we
        // both read (`req_slots`) and write (`resp_slots`,
        // `resp_ctrl`) through this mapping. `MAP_SHARED` is required
        // for writes to actually propagate back to the kernel's
        // backing memory rather than staying process-private
        // (`MAP_PRIVATE` would copy-on-write instead).
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                SHM_REGION_SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };

        if addr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }

        let region = addr as *mut KestrelfsSharedRegion;

        // SAFETY: `region` was just returned by a successful `mmap()`
        // of exactly `size_of::<KestrelfsSharedRegion>()` bytes, so
        // dereferencing it to read the two header fields below is
        // in-bounds. These are plain (non-atomic) reads of `magic`/
        // `abi_version`, which is sound here because nothing writes
        // to these two specific fields after the kernel module's
        // `kestrelfs_shm_alloc()` initializes them at module load time
        // (see `chardev.c`) - they are effectively immutable for the
        // lifetime of the mapping, unlike the ring control fields.
        let magic = unsafe { (*region).magic };
        if magic != crate::abi::SHM_MAGIC {
            // SAFETY: undo the mmap we just made before returning an
            // error, since no `KestrelDevice` will be constructed to
            // do it for us via `Drop`.
            unsafe {
                libc::munmap(addr, SHM_REGION_SIZE);
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "kestrelfs shared region magic mismatch: expected 0x{:08x}, got 0x{:08x} \
                     (mapped the wrong device, or kernel memory is corrupt)",
                    crate::abi::SHM_MAGIC,
                    magic
                ),
            ));
        }

        Ok(KestrelDevice { fd, region })
    }

    /// Issues `KESTRELFS_IOC_GET_ABI_VERSION` and returns the value
    /// the kernel module reports.
    fn ioctl_get_abi_version(fd: RawFd) -> io::Result<u32> {
        let mut version: u32 = 0;
        // SAFETY: `fd` is a valid, open file descriptor. `ioctl::GET_ABI_VERSION`
        // is an `_IOR` request expecting a `*mut u32` output argument
        // of exactly 4 bytes, matching `&mut version`'s type and size.
        // The kernel's handler (`kestrelfs_ioctl()` in `chardev.c`,
        // case `KESTRELFS_IOC_GET_ABI_VERSION`) writes exactly
        // `sizeof(__u32)` bytes via `copy_to_user()`, matching what we
        // provide here.
        let ret = unsafe { libc::ioctl(fd, ioctl::GET_ABI_VERSION as _, &mut version as *mut u32) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(version)
    }

    /// Issues `KESTRELFS_IOC_GET_REGION_SIZE` and returns the value the
    /// kernel module reports.
    fn ioctl_get_region_size(fd: RawFd) -> io::Result<u64> {
        let mut size: u64 = 0;
        // SAFETY: see `ioctl_get_abi_version` above; identical
        // reasoning, with an 8-byte `*mut u64` output argument
        // matching the kernel's `__u64` `copy_to_user()`.
        let ret = unsafe { libc::ioctl(fd, ioctl::GET_REGION_SIZE as _, &mut size as *mut u64) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(size)
    }

    /// Issues `KESTRELFS_IOC_NOTIFY_RESP`, telling the kernel that one
    /// or more new events have just been published into the RESP ring
    /// (see [`crate::ring::push_response`]). The kernel's handler bumps
    /// an internal generation counter and calls `wake_up_all()` on any
    /// kernel thread parked in `kestrelfs_wait_for_resp()`.
    pub fn notify_resp(&self) -> io::Result<()> {
        // SAFETY: `self.fd` is valid for the lifetime of `self` (see
        // struct invariants). `ioctl::NOTIFY_RESP` is a plain `_IO`
        // request with no argument, matching the kernel's
        // `KESTRELFS_IOC_NOTIFY_RESP` case in `kestrelfs_ioctl()`,
        // which reads no user pointer.
        let ret = unsafe { libc::ioctl(self.fd, ioctl::NOTIFY_RESP as _) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Durably retires every entry in the kernel-owned local cache. Redis
    /// metadata coherence uses this conservative operation when its shared
    /// revision changes; no cache bytes or metadata are passed through the
    /// shared bounce buffer.
    pub fn invalidate_cache_all(&self) -> io::Result<()> {
        // SAFETY: `self.fd` is live and INVALIDATE_CACHE_ALL is an `_IO`
        // command with no userspace pointer. The kernel serializes the cache
        // mutation against hit/fill/invalidate paths.
        let ret = unsafe { libc::ioctl(self.fd, ioctl::INVALIDATE_CACHE_ALL as _) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Returns the raw file descriptor, for use in a `libc::pollfd`
    /// (see `main.rs`'s event loop).
    pub fn as_raw_fd(&self) -> RawFd {
        self.fd
    }

    /// Returns a raw pointer to the mapped shared region, for use with
    /// [`crate::ring::drain_requests`]/[`crate::ring::push_response`].
    ///
    /// Deliberately returns a raw pointer rather than a safe reference,
    /// see the module doc comment on `ring.rs` for why a safe `&`/
    /// `&mut KestrelfsSharedRegion` would be unsound here (the kernel
    /// concurrently mutates parts of this memory outside Rust's
    /// knowledge).
    pub fn region_ptr(&self) -> *mut KestrelfsSharedRegion {
        self.region
    }
}

impl Drop for KestrelDevice {
    /// Unmaps the shared region and closes the device fd.
    ///
    /// Ordering matches the natural teardown sequence: unmap first
    /// (the mapping is only meaningful while the fd is open, though
    /// POSIX does not actually require unmapping before closing), then
    /// close. Errors from either syscall are deliberately ignored here,
    /// since `Drop::drop` cannot propagate a `Result`, and by the time
    /// we are tearing down there is no meaningful recovery action the
    /// daemon could take beyond what it would already do on process
    /// exit.
    fn drop(&mut self) {
        // SAFETY: `self.region` is guaranteed by the struct invariant
        // to be a live mapping of exactly `SHM_REGION_SIZE` bytes,
        // created by this same struct's constructor and never handed
        // out as an owning pointer elsewhere, so unmapping it here is
        // sound and cannot race with any other `munmap()` of the same
        // address.
        unsafe {
            libc::munmap(self.region as *mut libc::c_void, SHM_REGION_SIZE);
        }
        // SAFETY: `self.fd` is guaranteed by the struct invariant to
        // be an open fd owned solely by this struct.
        unsafe {
            libc::close(self.fd);
        }
    }
}
