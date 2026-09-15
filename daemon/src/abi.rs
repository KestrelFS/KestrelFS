// SPDX-License-Identifier: GPL-2.0 WITH Linux-syscall-note
//! ABI mirror of `kestrelfs_ipc.h`.
//!
//! Every type in this module MUST byte-for-byte match its C counterpart
//! defined in `kestrelfs/kestrelfs_ipc.h`. This is the single most
//! safety-critical file in the daemon: if these layouts ever drift from
//! the kernel header, the daemon will silently misinterpret shared
//! kernel memory, corrupting the ring buffers or reading garbage.
//!
//! # Why hand-written instead of `bindgen`
//!
//! A `bindgen`-generated version of this file would be strictly safer
//! against drift, and is the intended long-term approach (see the
//! `build.rs` note at the bottom of this file). For this Phase 2 step we
//! hand-mirror the header instead, deliberately keeping this file small
//! and heavily commented so every field can be checked by eye against
//! `kestrelfs_ipc.h` line-by-line. [`compile_time_layout_asserts`] then
//! pins every `size_of`/`align_of`/offset invariant at compile time,
//! exactly mirroring the `_Static_assert`s at the bottom of the C
//! header, so any accidental drift fails the Rust build loudly instead
//! of corrupting shared memory silently at runtime.
//!
//! # Why `#[repr(C)]` without `#[repr(packed)]`
//!
//! The C header is deliberately written with explicit padding fields and
//! natural alignment (see its "CROSS-LANGUAGE ALIGNMENT CONTRACT"
//! section) specifically so that plain `#[repr(C)]` - which uses the
//! platform's normal alignment rules, identical to what a C compiler
//! does for a non-packed struct - reproduces an identical byte layout.
//! `#[repr(packed)]` is avoided on purpose: it forces byte-unaligned
//! loads/stores for multi-byte fields, which is both slower and, for
//! `AtomicU64`, outright undefined behavior on some platforms (atomic
//! ops generally require natural alignment).

use std::sync::atomic::{AtomicU32, AtomicU64};

/// Number of slots per ring. Mirrors `KESTRELFS_RING_SLOTS` in
/// `kestrelfs_ipc.h`. MUST be a power of two.
pub const RING_SLOTS: usize = 1024;

/// Mirrors `KESTRELFS_RING_MASK`.
pub const RING_MASK: u64 = (RING_SLOTS - 1) as u64;

/// Mirrors `KESTRELFS_CACHELINE_SIZE`.
pub const CACHELINE_SIZE: usize = 64;

/// Mirrors `KESTRELFS_EVENT_PAYLOAD_SIZE`.
pub const EVENT_PAYLOAD_SIZE: usize = 32;

/// Size of the serialized bulk-I/O bounce buffer appended after both rings.
/// Mirrors `KESTRELFS_DATA_BUFFER_SIZE` (16 KiB).
pub const DATA_BUFFER_SIZE: usize = 16 * 1024;

/// Mirrors `KESTRELFS_ABI_VERSION`. The daemon refuses to attach to a
/// kernel module reporting any other value (see [`super::device::open`]).
pub const ABI_VERSION: u32 = 15;

/// Mirrors `KESTRELFS_SHM_MAGIC` ("KSRS" packed into a little-endian u32).
pub const SHM_MAGIC: u32 = 0x4B53_5253;

// ---------------------------------------------------------------------
// Opcodes - mirrors the `KESTRELFS_OP_*` #defines.
// ---------------------------------------------------------------------
//
// Every opcode from `kestrelfs_ipc.h` is mirrored here for ABI
// completeness even though this Phase 2 bootstrap daemon only ever
// produces/consumes `OP_NOP` and `OP_RESULT_OK` (see `main.rs`) - the
// remaining opcodes will be exercised once real VFS call paths
// (Phase 2's VFS integration step, and Phase 3's chunk/metadata logic)
// start issuing `OP_LOOKUP`/`OP_READ_CHUNK`/`OP_GETATTR` requests.

/// No-op / padding event. Mirrors `KESTRELFS_OP_NOP`.
#[allow(dead_code)]
pub const OP_NOP: u32 = 0;
/// Request: resolve path -> inode metadata. Mirrors `KESTRELFS_OP_LOOKUP`.
#[allow(dead_code)]
pub const OP_LOOKUP: u32 = 1;
/// Request: fetch chunk data (cache miss). Mirrors `KESTRELFS_OP_READ_CHUNK`.
#[allow(dead_code)]
pub const OP_READ_CHUNK: u32 = 2;
/// Request: fetch inode attributes. Mirrors `KESTRELFS_OP_GETATTR`.
#[allow(dead_code)]
pub const OP_GETATTR: u32 = 3;
/// Request: write data to file. Mirrors `KESTRELFS_OP_WRITE_CHUNK`.
pub const OP_WRITE_CHUNK: u32 = 4;
/// Request: set file size (truncate/ftruncate). Mirrors `KESTRELFS_OP_TRUNCATE`.
pub const OP_TRUNCATE: u32 = 5;
/// Request: create new file/directory. Mirrors `KESTRELFS_OP_CREATE`.
pub const OP_CREATE: u32 = 6;
/// Request: list directory entries. Mirrors `KESTRELFS_OP_READDIR`.
pub const OP_READDIR: u32 = 7;
/// Request: create new directory. Mirrors `KESTRELFS_OP_MKDIR`.
pub const OP_MKDIR: u32 = 8;
/// Request: remove file or directory. Mirrors `KESTRELFS_OP_UNLINK`.
pub const OP_UNLINK: u32 = 9;
/// Request: rename/move file or directory. Mirrors `KESTRELFS_OP_RENAME`.
pub const OP_RENAME: u32 = 10;
/// Request: write bytes from the shared data bounce buffer.
pub const OP_WRITE_DATA: u32 = 11;
/// Request: read bytes into the shared data bounce buffer.
pub const OP_READ_DATA: u32 = 12;
/// Request: rename/move using names in the shared data bounce buffer.
pub const OP_RENAME_DATA: u32 = 13;
/// Request: lookup a bounce-buffer name.
pub const OP_LOOKUP_DATA: u32 = 14;
/// Request: create a file whose name is in the bounce buffer.
pub const OP_CREATE_DATA: u32 = 15;
/// Request: create a directory whose name is in the bounce buffer.
pub const OP_MKDIR_DATA: u32 = 16;
/// Request: unlink a bounce-buffer name.
pub const OP_UNLINK_DATA: u32 = 17;
/// Request: return batched directory entries through the bounce buffer.
pub const OP_READDIR_DATA: u32 = 18;
/// Request: create a symbolic link using name and target in the bounce buffer.
pub const OP_SYMLINK_DATA: u32 = 19;
/// Request: return a symbolic-link target through the bounce buffer.
pub const OP_READLINK_DATA: u32 = 20;
/// Request: create a hard link whose new name is in the bounce buffer.
pub const OP_LINK_DATA: u32 = 21;
/// Request: reclaim an unlinked inode after its final open handle closes.
pub const OP_FINALIZE_ORPHAN: u32 = 22;
/// Response: generic success. Mirrors `KESTRELFS_OP_RESULT_OK`.
pub const OP_RESULT_OK: u32 = 64;
/// Response: generic failure, see `error_code`. Mirrors
/// `KESTRELFS_OP_RESULT_ERROR`.
#[allow(dead_code)]
pub const OP_RESULT_ERROR: u32 = 65;

// ---------------------------------------------------------------------
// struct kestrelfs_event
// ---------------------------------------------------------------------

/// Mirrors `struct kestrelfs_event` in `kestrelfs_ipc.h`.
///
/// Field-for-field layout (must total exactly 64 bytes, one cacheline):
///
/// ```text
/// seq          u64   offset  0   sequence number assigned by producer
/// opcode       u32   offset  8   one of the OP_* constants above
/// flags        u32   offset 12   reserved, must be zero for now
/// req_id       u64   offset 16   unique request id, echoed back on RESP
/// error_code   i32   offset 24   0 = success, negative errno on failure
/// _reserved0   u32   offset 28   padding to keep req_id/payload aligned
/// payload      [u8;32] offset 32 opcode-specific raw bytes
/// ------------------------------------------------------------------
/// total: 64 bytes
/// ```
///
/// `#[derive(Clone, Copy)]` is safe and intentional: this type is a pure
/// value type over shared memory, must never own any Rust-managed
/// resource (no `String`, `Vec`, `Box`, etc.), and is frequently copied
/// out of the mmap'd region into a local stack value before being
/// inspected - copying is exactly the safe way to read a shared-memory
/// slot without holding a live reference into memory another party
/// (the kernel) can mutate concurrently.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct KestrelfsEvent {
    pub seq: u64,
    pub opcode: u32,
    pub flags: u32,
    pub req_id: u64,
    pub error_code: i32,
    pub _reserved0: u32,
    pub payload: [u8; EVENT_PAYLOAD_SIZE],
}

impl KestrelfsEvent {
    /// Builds a zeroed event with `opcode`/`req_id` set, matching how
    /// `kestrelfs_req_push()` in `ipc_ring.c` fills a freshly claimed
    /// slot (`seq` is set separately by the caller, mirroring the C
    /// side's `slot->seq = head`).
    pub fn zeroed(opcode: u32, req_id: u64) -> Self {
        KestrelfsEvent {
            seq: 0,
            opcode,
            flags: 0,
            req_id,
            error_code: 0,
            _reserved0: 0,
            payload: [0u8; EVENT_PAYLOAD_SIZE],
        }
    }

    /// Decodes this event's payload as a `KESTRELFS_OP_READ_CHUNK`
    /// request, per the byte layout documented in `kestrelfs_ipc.h`'s
    /// "Payload layout for KESTRELFS_OP_READ_CHUNK requests" section.
    /// As of KESTRELFS_ABI_VERSION 2, the layout is: little-endian u64
    /// inode_id at byte 0, u64 offset at byte 8, u32 count at byte 16
    /// (v1 had no inode_id field and placed offset/count at bytes 0/8 -
    /// this decoder only handles v2).
    ///
    /// Callers are expected to have already checked `self.opcode ==
    /// OP_READ_CHUNK` - this method does not itself inspect `opcode`,
    /// since the payload bytes are meaningless without already
    /// knowing which opcode produced them.
    pub fn decode_read_chunk_req(&self) -> ReadChunkReq {
        ReadChunkReq {
            inode_id: u64::from_le_bytes(self.payload[0..8].try_into().unwrap()),
            offset: u64::from_le_bytes(self.payload[8..16].try_into().unwrap()),
            count: u32::from_le_bytes(self.payload[16..20].try_into().unwrap()),
        }
    }

    /// Decodes the payload as a `KESTRELFS_OP_WRITE_CHUNK` request (ABI v3).
    ///
    /// Layout:
    /// - inode_id: u64 at offset 0
    /// - offset: u64 at offset 8
    /// - count: u32 at offset 16 (max 12 bytes)
    /// - data: bytes at offset 20
    ///
    /// Caller must have already checked `opcode == OP_WRITE_CHUNK`.
    pub fn decode_write_chunk_req(&self) -> WriteChunkReq {
        let inode_id = u64::from_le_bytes(self.payload[0..8].try_into().unwrap());
        let offset = u64::from_le_bytes(self.payload[8..16].try_into().unwrap());
        let count = u32::from_le_bytes(self.payload[16..20].try_into().unwrap());
        let count = count.min(12);
        let data = self.payload[20..20 + count as usize].to_vec();

        WriteChunkReq {
            inode_id,
            offset,
            count,
            data,
        }
    }

    /// Decodes the common ABI v8 `OP_WRITE_DATA` / `OP_READ_DATA`
    /// request payload: inode_id@0, file_offset@8, length@16.
    pub fn decode_data_req(&self) -> DataReq {
        DataReq {
            inode_id: u64::from_le_bytes(self.payload[0..8].try_into().unwrap()),
            offset: u64::from_le_bytes(self.payload[8..16].try_into().unwrap()),
            length: u32::from_le_bytes(self.payload[16..20].try_into().unwrap()),
        }
    }

    /// Decodes a `KESTRELFS_OP_TRUNCATE` request from the payload.
    ///
    /// Payload layout (little-endian):
    ///   [0..8)   inode_id (u64)
    ///   [8..16)  new_size (u64)
    pub fn decode_truncate_req(&self) -> TruncateReq {
        let inode_id = u64::from_le_bytes(self.payload[0..8].try_into().unwrap());
        let new_size = u64::from_le_bytes(self.payload[8..16].try_into().unwrap());

        TruncateReq {
            inode_id,
            new_size,
        }
    }

    /// Decodes this event's payload as a `KESTRELFS_OP_CREATE` request:
    /// parent_inode_id (8 bytes) + mode (4 bytes) + filename (20 bytes).
    ///
    /// Layout per `kestrelfs_ipc.h`:
    ///   offset 0: parent_inode_id (u64)
    ///   offset 8: mode (u32)
    ///   offset 12: name (NUL-terminated, max 19 chars + NUL)
    pub fn decode_create_req(&self) -> Result<CreateReq, LookupDecodeError> {
        let parent_inode = u64::from_le_bytes(self.payload[0..8].try_into().unwrap());
        let mode = u32::from_le_bytes(self.payload[8..12].try_into().unwrap());

        // Find NUL terminator in name field (bytes 12..32)
        let name_bytes = &self.payload[12..32];
        let name_len = name_bytes.iter().position(|&b| b == 0).unwrap_or(20);

        if name_len == 0 {
            return Err(LookupDecodeError::InvalidUtf8);
        }

        let name = std::str::from_utf8(&name_bytes[..name_len])
            .map_err(|_| LookupDecodeError::InvalidUtf8)?
            .to_string();

        Ok(CreateReq {
            parent_inode,
            mode,
            name,
        })
    }

    /// Decodes this event's payload as a `KESTRELFS_OP_READDIR` request:
    /// dir_inode_id (8 bytes) + offset (4 bytes).
    ///
    /// Layout per `kestrelfs_ipc.h`:
    ///   offset 0: dir_inode_id (u64)
    ///   offset 8: offset (u32, entry index to start from)
    ///   offset 12..32: reserved
    pub fn decode_readdir_req(&self) -> ReaddirReq {
        let dir_inode = u64::from_le_bytes(self.payload[0..8].try_into().unwrap());
        let offset = u32::from_le_bytes(self.payload[8..12].try_into().unwrap());

        ReaddirReq { dir_inode, offset }
    }

    /// Decodes a `KESTRELFS_OP_RENAME` request payload.
    ///
    /// Wire layout (32 bytes total):
    ///   old_parent(8) + new_parent(8) + old_name_len(1) + new_name_len(1)
    ///   + old_name(7) + new_name(7)
    ///
    /// Returns `Err` if either name length exceeds [`RENAME_NAME_MAX`] or
    /// if either name is invalid UTF-8.
    pub fn decode_rename_req(&self) -> Result<RenameReq, RenameDecodeError> {
        let old_parent = u64::from_le_bytes(self.payload[0..8].try_into().unwrap());
        let new_parent = u64::from_le_bytes(self.payload[8..16].try_into().unwrap());
        let old_name_len = self.payload[16];
        let new_name_len = self.payload[17];

        if old_name_len as usize > RENAME_NAME_MAX {
            return Err(RenameDecodeError::OldNameTooLong(old_name_len));
        }
        if new_name_len as usize > RENAME_NAME_MAX {
            return Err(RenameDecodeError::NewNameTooLong(new_name_len));
        }

        let old_name_bytes = &self.payload[18..18 + old_name_len as usize];
        let new_name_bytes = &self.payload[25..25 + new_name_len as usize];

        let old_name = std::str::from_utf8(old_name_bytes)
            .map_err(|_| RenameDecodeError::OldNameInvalidUtf8)?
            .to_string();
        let new_name = std::str::from_utf8(new_name_bytes)
            .map_err(|_| RenameDecodeError::NewNameInvalidUtf8)?
            .to_string();

        Ok(RenameReq {
            old_parent,
            new_parent,
            old_name,
            new_name,
            flags: 0,
            defer_reclaim: false,
        })
    }

    /// Decodes an ABI v9 `KESTRELFS_OP_RENAME_DATA` request.
    ///
    /// The payload carries `old_parent@0`, `new_parent@8`, and two little-endian
    /// `u16` lengths at offsets 16 and 18, plus rename flags at offset 20. The
    /// old and new names are adjacent at the beginning of the shared data
    /// bounce buffer.
    ///
    /// # Safety
    ///
    /// `data_buffer` must point to the start of a live
    /// [`DATA_BUFFER_SIZE`]-byte shared bounce buffer. The caller must ensure
    /// exclusive data-IPC ownership until this method returns.
    pub unsafe fn decode_rename_data_req(
        &self,
        data_buffer: *const u8,
    ) -> Result<RenameReq, RenameDataDecodeError> {
        if self.flags != 0 {
            return Err(RenameDataDecodeError::UnsupportedFlags(self.flags));
        }

        let old_parent = u64::from_le_bytes(self.payload[0..8].try_into().unwrap());
        let new_parent = u64::from_le_bytes(self.payload[8..16].try_into().unwrap());
        let old_name_len = u16::from_le_bytes(self.payload[16..18].try_into().unwrap());
        let new_name_len = u16::from_le_bytes(self.payload[18..20].try_into().unwrap());
        let flags = u32::from_le_bytes(self.payload[20..24].try_into().unwrap());
        let lifecycle_flags = u32::from_le_bytes(self.payload[24..28].try_into().unwrap());

        if flags & !RENAME_NOREPLACE != 0 {
            return Err(RenameDataDecodeError::UnsupportedRenameFlags(flags));
        }
        if lifecycle_flags & !LIFECYCLE_DEFER_RECLAIM != 0 {
            return Err(RenameDataDecodeError::UnsupportedLifecycleFlags(lifecycle_flags));
        }

        if old_name_len as usize > RENAME_DATA_NAME_MAX {
            return Err(RenameDataDecodeError::OldNameTooLong(old_name_len));
        }
        if new_name_len as usize > RENAME_DATA_NAME_MAX {
            return Err(RenameDataDecodeError::NewNameTooLong(new_name_len));
        }

        let total_len = (old_name_len as usize)
            .checked_add(new_name_len as usize)
            .ok_or(RenameDataDecodeError::CombinedNamesTooLong)?;
        if total_len > DATA_BUFFER_SIZE {
            return Err(RenameDataDecodeError::CombinedNamesTooLong);
        }

        let mut names = vec![0u8; total_len];
        // SAFETY: guaranteed by this function's contract. Lengths have been
        // checked against the mapped buffer size, and `names` owns total_len
        // initialized writable bytes. The kernel serializes ownership.
        unsafe {
            std::ptr::copy_nonoverlapping(data_buffer, names.as_mut_ptr(), total_len);
        }

        let split = old_name_len as usize;
        let old_name = std::str::from_utf8(&names[..split])
            .map_err(|_| RenameDataDecodeError::OldNameInvalidUtf8)?
            .to_string();
        let new_name = std::str::from_utf8(&names[split..])
            .map_err(|_| RenameDataDecodeError::NewNameInvalidUtf8)?
            .to_string();

        Ok(RenameReq {
            old_parent,
            new_parent,
            old_name,
            new_name,
            flags,
            defer_reclaim: lifecycle_flags & LIFECYCLE_DEFER_RECLAIM != 0,
        })
    }

    /// Decodes the common ABI v10 single-name DATA request.
    ///
    /// # Safety
    ///
    /// `data_buffer` must point to a live [`DATA_BUFFER_SIZE`]-byte bounce
    /// buffer exclusively owned by this request until decoding completes.
    pub unsafe fn decode_name_data_req(
        &self,
        data_buffer: *const u8,
    ) -> Result<NameDataReq, NameDataDecodeError> {
        if self.flags != 0 {
            return Err(NameDataDecodeError::UnsupportedFlags(self.flags));
        }

        let parent_inode = u64::from_le_bytes(self.payload[0..8].try_into().unwrap());
        let name_len = u16::from_le_bytes(self.payload[8..10].try_into().unwrap());
        let mode = u32::from_le_bytes(self.payload[12..16].try_into().unwrap());
        if name_len == 0 {
            return Err(NameDataDecodeError::EmptyName);
        }
        if name_len as usize > NAME_DATA_MAX {
            return Err(NameDataDecodeError::NameTooLong(name_len));
        }

        let mut name_bytes = vec![0u8; name_len as usize];
        // SAFETY: guaranteed by this function's contract; name_len has been
        // bounded by both NAME_DATA_MAX and the bounce-buffer size.
        unsafe {
            std::ptr::copy_nonoverlapping(
                data_buffer,
                name_bytes.as_mut_ptr(),
                name_len as usize,
            );
        }
        let name = String::from_utf8(name_bytes).map_err(|_| NameDataDecodeError::InvalidUtf8)?;

        Ok(NameDataReq {
            parent_inode,
            mode,
            name,
        })
    }

    /// Decodes an ABI v12 hard-link request. The fixed payload carries the
    /// destination parent and existing inode id; the new name is stored at
    /// the beginning of the serialized bounce buffer.
    ///
    /// # Safety
    ///
    /// `data_buffer` must point to a live [`DATA_BUFFER_SIZE`]-byte bounce
    /// buffer exclusively owned by this synchronous request.
    pub unsafe fn decode_link_data_req(
        &self,
        data_buffer: *const u8,
    ) -> Result<LinkDataReq, NameDataDecodeError> {
        if self.flags != 0 {
            return Err(NameDataDecodeError::UnsupportedFlags(self.flags));
        }
        let parent_inode = u64::from_le_bytes(self.payload[0..8].try_into().unwrap());
        let inode_id = u64::from_le_bytes(self.payload[8..16].try_into().unwrap());
        let name_len = u16::from_le_bytes(self.payload[16..18].try_into().unwrap());
        if name_len == 0 {
            return Err(NameDataDecodeError::EmptyName);
        }
        if name_len as usize > NAME_DATA_MAX {
            return Err(NameDataDecodeError::NameTooLong(name_len));
        }

        let mut name_bytes = vec![0u8; name_len as usize];
        // SAFETY: guaranteed by this function's contract and the checked ABI
        // name bound above.
        unsafe {
            std::ptr::copy_nonoverlapping(
                data_buffer,
                name_bytes.as_mut_ptr(),
                name_len as usize,
            );
        }
        let name = String::from_utf8(name_bytes).map_err(|_| NameDataDecodeError::InvalidUtf8)?;
        Ok(LinkDataReq {
            parent_inode,
            inode_id,
            name,
        })
    }

    /// Decodes an ABI v11 `OP_SYMLINK_DATA` request. The bounce buffer holds
    /// the name immediately followed by the target.
    ///
    /// # Safety
    ///
    /// `data_buffer` must point to a live, exclusively owned
    /// [`DATA_BUFFER_SIZE`]-byte buffer.
    pub unsafe fn decode_symlink_data_req(
        &self,
        data_buffer: *const u8,
    ) -> Result<SymlinkDataReq, SymlinkDataDecodeError> {
        if self.flags != 0 {
            return Err(SymlinkDataDecodeError::UnsupportedFlags(self.flags));
        }
        let parent_inode = u64::from_le_bytes(self.payload[0..8].try_into().unwrap());
        let name_len = u16::from_le_bytes(self.payload[8..10].try_into().unwrap());
        let target_len = u16::from_le_bytes(self.payload[10..12].try_into().unwrap());
        if name_len == 0 {
            return Err(SymlinkDataDecodeError::EmptyName);
        }
        if name_len as usize > NAME_DATA_MAX {
            return Err(SymlinkDataDecodeError::NameTooLong(name_len));
        }
        if target_len == 0 {
            return Err(SymlinkDataDecodeError::EmptyTarget);
        }
        if target_len as usize > SYMLINK_TARGET_MAX {
            return Err(SymlinkDataDecodeError::TargetTooLong(target_len));
        }
        let total_len = name_len as usize + target_len as usize;
        if total_len > DATA_BUFFER_SIZE {
            return Err(SymlinkDataDecodeError::CombinedDataTooLong);
        }
        let mut bytes = vec![0; total_len];
        // SAFETY: the checked lengths fit the buffer promised by the caller.
        unsafe { std::ptr::copy_nonoverlapping(data_buffer, bytes.as_mut_ptr(), total_len) };
        let split = name_len as usize;
        let name = std::str::from_utf8(&bytes[..split])
            .map_err(|_| SymlinkDataDecodeError::NameInvalidUtf8)?
            .to_string();
        let target = std::str::from_utf8(&bytes[split..])
            .map_err(|_| SymlinkDataDecodeError::TargetInvalidUtf8)?
            .to_string();
        Ok(SymlinkDataReq {
            parent_inode,
            name,
            target,
        })
    }

    /// Builds a `KESTRELFS_OP_RESULT_OK` response for a
    /// `KESTRELFS_OP_READ_CHUNK` request, with `data` copied into the
    /// leading bytes of the payload and the remainder zero-padded, per
    /// the "no explicit length field" convention documented in
    /// `kestrelfs_ipc.h` (the kernel consumer clamps its own
    /// `copy_to_user()` to the count it originally requested, which is
    /// always `<= data.len()` here by construction - see
    /// `main.rs::handle_read_chunk`).
    ///
    /// # Panics
    ///
    /// Panics if `data.len() > EVENT_PAYLOAD_SIZE` - this indicates a
    /// bug in the caller, not a runtime/protocol condition, since
    /// `data` is always produced by clamping to
    /// `KESTRELFS_READ_CHUNK_MAX_LEN` (== `EVENT_PAYLOAD_SIZE`) before
    /// reaching this function.
    pub fn read_chunk_response(req_id: u64, data: &[u8]) -> Self {
        assert!(
            data.len() <= EVENT_PAYLOAD_SIZE,
            "read_chunk_response: data.len()={} exceeds EVENT_PAYLOAD_SIZE={}",
            data.len(),
            EVENT_PAYLOAD_SIZE
        );

        let mut event = KestrelfsEvent::zeroed(OP_RESULT_OK, req_id);
        event.payload[..data.len()].copy_from_slice(data);
        event
    }

    /// Builds a `KESTRELFS_OP_RESULT_ERROR` response, with `errno`
    /// (a negative errno-style value, matching the kernel consumer's
    /// expectations - see `kestrelfs_remote_read()` in `file.c`)
    /// placed in `error_code`.
    ///
    /// Not yet exercised by this Phase 2 bootstrap daemon (see
    /// `main.rs::handle_read_chunk`, which currently always succeeds).
    /// Kept ready for the first opcode handler that needs to report
    /// a real failure (e.g. an out-of-range offset once `remote.txt`
    /// gains an actual bounded backing store in a later phase).
    #[allow(dead_code)]
    pub fn error_response(req_id: u64, errno: i32) -> Self {
        let mut event = KestrelfsEvent::zeroed(OP_RESULT_ERROR, req_id);
        event.error_code = errno;
        event
    }

    /// Decodes this event's payload as a `KESTRELFS_OP_LOOKUP`
    /// request, per the byte layout documented in `kestrelfs_ipc.h`'s
    /// "Payload layout for KESTRELFS_OP_LOOKUP requests/responses"
    /// section: a little-endian `u64` parent inode at byte 0, a `u8`
    /// name length at byte 8, followed by that many name bytes at
    /// byte 9.
    ///
    /// Callers are expected to have already checked `self.opcode ==
    /// OP_LOOKUP`, exactly like [`Self::decode_read_chunk_req`].
    ///
    /// # Errors
    ///
    /// [`LookupDecodeError::NameTooLong`] if the wire `name_len` byte
    /// exceeds [`LOOKUP_NAME_MAX`] (a malformed payload - a correct
    /// producer never sends one). [`LookupDecodeError::InvalidUtf8`]
    /// if the name bytes are not valid UTF-8.
    pub fn decode_lookup_req(&self) -> Result<LookupReq, LookupDecodeError> {
        let parent_inode = u64::from_le_bytes(self.payload[0..8].try_into().unwrap());
        let name_len = self.payload[8];

        if name_len as usize > LOOKUP_NAME_MAX {
            return Err(LookupDecodeError::NameTooLong(name_len));
        }

        let name_bytes = &self.payload[9..9 + name_len as usize];
        let name = std::str::from_utf8(name_bytes)
            .map_err(|_| LookupDecodeError::InvalidUtf8)?
            .to_string();

        Ok(LookupReq { parent_inode, name })
    }

    /// Decodes this event's payload as a `KESTRELFS_OP_GETATTR`
    /// request: a little-endian `u64` inode id at byte 0 (bytes
    /// 8..32 are reserved/ignored, per `kestrelfs_ipc.h`).
    ///
    /// Callers are expected to have already checked `self.opcode ==
    /// OP_GETATTR`.
    pub fn decode_getattr_req(&self) -> GetattrReq {
        GetattrReq {
            inode_id: u64::from_le_bytes(self.payload[0..8].try_into().unwrap()),
        }
    }

    /// Builds a `KESTRELFS_OP_RESULT_OK` response for a
    /// `KESTRELFS_OP_LOOKUP` request, per the exact byte layout
    /// documented in `kestrelfs_ipc.h`: `child_inode_id`(8) +
    /// `size`(8) + `mode`(4) + `uid`(4) + `gid`(4) + `nlink`(4),
    /// summing to exactly [`EVENT_PAYLOAD_SIZE`] (32) bytes - every
    /// byte of the payload is meaningful for this response, unlike
    /// [`Self::read_chunk_response`]'s zero-padded tail.
    pub fn lookup_response(req_id: u64, child_inode_id: u64, attrs: AttrFields) -> Self {
        let mut event = KestrelfsEvent::zeroed(OP_RESULT_OK, req_id);
        let p = &mut event.payload;
        p[0..8].copy_from_slice(&child_inode_id.to_le_bytes());
        p[8..16].copy_from_slice(&attrs.size.to_le_bytes());
        p[16..20].copy_from_slice(&attrs.mode.to_le_bytes());
        p[20..24].copy_from_slice(&attrs.uid.to_le_bytes());
        p[24..28].copy_from_slice(&attrs.gid.to_le_bytes());
        p[28..32].copy_from_slice(&attrs.nlink.to_le_bytes());
        event
    }

    /// Builds a `KESTRELFS_OP_RESULT_OK` response for a
    /// `KESTRELFS_OP_GETATTR` request, per the exact byte layout
    /// documented in `kestrelfs_ipc.h`: `size`(8) + `mode`(4) +
    /// `uid`(4) + `gid`(4) + `nlink`(4) + `mtime`(8), summing to
    /// exactly [`EVENT_PAYLOAD_SIZE`] (32) bytes. Note this response
    /// does NOT repeat an inode id (see `kestrelfs_ipc.h`'s doc
    /// comment on why: the requester already supplied it and matches
    /// via `req_id`), freeing up room for `mtime` within the same
    /// 32-byte budget [`Self::lookup_response`] spends on
    /// `child_inode_id` instead.
    pub fn getattr_response(req_id: u64, attrs: AttrFields) -> Self {
        let mut event = KestrelfsEvent::zeroed(OP_RESULT_OK, req_id);
        let p = &mut event.payload;
        p[0..8].copy_from_slice(&attrs.size.to_le_bytes());
        p[8..12].copy_from_slice(&attrs.mode.to_le_bytes());
        p[12..16].copy_from_slice(&attrs.uid.to_le_bytes());
        p[16..20].copy_from_slice(&attrs.gid.to_le_bytes());
        p[20..24].copy_from_slice(&attrs.nlink.to_le_bytes());
        p[24..32].copy_from_slice(&attrs.mtime.to_le_bytes());
        event
    }

    /// Builds a `KESTRELFS_OP_RESULT_OK` response for a
    /// `KESTRELFS_OP_READDIR` request.
    ///
    /// Layout per `kestrelfs_ipc.h`:
    ///   offset 0: entry_count (u8, 0-2)
    ///   offset 1: entry[0].inode_id (u64)
    ///   offset 9: entry[0].name (11 bytes, NUL-terminated)
    ///   offset 20: entry[1].inode_id (u64)
    ///   offset 28: entry[1].name (4 bytes, truncated)
    ///
    /// Due to payload size constraints, we can fit at most 2 entries per response.
    /// entry_count=0 signals end-of-directory.
    pub fn readdir_response(req_id: u64, entries: &[(u64, &str)]) -> Self {
        let mut event = KestrelfsEvent::zeroed(OP_RESULT_OK, req_id);
        let p = &mut event.payload;

        // Limit to 1 entry per response to avoid name truncation.
        // With 32-byte payload, returning 2 entries leaves only 4 bytes
        // for the second name (entry[1]@28), causing "remote.txt" -> "remo".
        // Solution: return max 1 entry with full name space (~20 bytes).
        let entry_count = entries.len().min(1) as u8;
        p[0] = entry_count;

        if entry_count >= 1 {
            let (inode, name) = entries[0];
            p[1..9].copy_from_slice(&inode.to_le_bytes());
            let name_bytes = name.as_bytes();
            // With only 1 entry, we can use more space for the name.
            // Layout: entry_count(1) + inode(8) + name(up to 23 bytes)
            // Total: 1 + 8 + 23 = 32 (fits in payload)
            let copy_len = name_bytes.len().min(23);
            p[9..9 + copy_len].copy_from_slice(&name_bytes[..copy_len]);
            // NUL terminator if space allows
            if copy_len < 23 {
                p[9 + copy_len] = 0;
            }
        }

        event
    }
}

/// Mirrors `KESTRELFS_LOOKUP_NAME_MAX` in `kestrelfs_ipc.h`: the
/// maximum number of bytes a `KESTRELFS_OP_LOOKUP` request's `name`
/// field may carry (see that macro's doc comment in the C header for
/// the exact wire layout this bounds: `parent_inode`(8) +
/// `name_len`(1) + `name`(this many bytes) must fit within
/// [`EVENT_PAYLOAD_SIZE`]).
pub const LOOKUP_NAME_MAX: usize = 23;

/// Mirrors `KESTRELFS_RENAME_NAME_MAX` in `kestrelfs_ipc.h`: the
/// maximum number of bytes each name in a `KESTRELFS_OP_RENAME` request
/// may carry. Wire layout: old_parent(8) + new_parent(8) + old_name_len(1)
/// + new_name_len(1) + old_name(7) + new_name(7) = 32 bytes.
pub const RENAME_NAME_MAX: usize = 7;

/// Mirrors `KESTRELFS_RENAME_DATA_NAME_MAX`: ABI v9 permits each name in an
/// `OP_RENAME_DATA` request to contain up to POSIX `NAME_MAX` bytes.
pub const RENAME_DATA_NAME_MAX: usize = 255;
/// RENAME_DATA payload flag matching Linux `RENAME_NOREPLACE`.
pub const RENAME_NOREPLACE: u32 = 1;
/// Defer inode/object reclamation until `OP_FINALIZE_ORPHAN`.
pub const LIFECYCLE_DEFER_RECLAIM: u32 = 1;

/// ABI v10 maximum for LOOKUP_DATA/CREATE_DATA/MKDIR_DATA/UNLINK_DATA names
/// and READDIR_DATA entry names. Mirrors `KESTRELFS_NAME_DATA_MAX`.
pub const NAME_DATA_MAX: usize = 255;

/// Bytes preceding each variable-length READDIR_DATA entry name: inode u64
/// followed by name_len u16.
pub const READDIR_DATA_ENTRY_HEADER_SIZE: usize = 10;
/// Maximum UTF-8 symlink target size, mirrored from the C ABI.
pub const SYMLINK_TARGET_MAX: usize = 4095;

/// Decoded form of a `KESTRELFS_OP_LOOKUP` request payload. See
/// [`KestrelfsEvent::decode_lookup_req`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookupReq {
    pub parent_inode: u64,
    pub name: String,
}

/// Errors [`KestrelfsEvent::decode_lookup_req`] can report.
///
/// Both variants indicate a malformed wire payload - under normal
/// operation (a correctly-implemented producer respecting
/// [`LOOKUP_NAME_MAX`]), neither should ever actually occur; they
/// exist so a handler can defensively map a corrupt/malicious payload
/// to a sensible error response instead of panicking or silently
/// misinterpreting garbage bytes as a name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LookupDecodeError {
    /// The wire `name_len` byte exceeded [`LOOKUP_NAME_MAX`]. A
    /// correct producer must reject an over-length name at its own
    /// call site (surfacing `-ENAMETOOLONG` to whatever caller asked
    /// it to look up that name) rather than ever placing `name_len >
    /// LOOKUP_NAME_MAX` on the wire in the first place - see
    /// `KESTRELFS_LOOKUP_NAME_MAX`'s doc comment in `kestrelfs_ipc.h`.
    #[error("name_len {0} exceeds LOOKUP_NAME_MAX ({LOOKUP_NAME_MAX})")]
    NameTooLong(u8),
    /// The `name_len` bytes at their wire offset were not valid
    /// UTF-8. The wire format itself does not require UTF-8 (see the
    /// "Payload layout for KESTRELFS_OP_LOOKUP" section in
    /// `kestrelfs_ipc.h`), but every current `MetaStore` consumer
    /// (see `meta.rs`) operates on `&str` names, so a non-UTF-8 name
    /// cannot be resolved by this daemon today.
    #[error("name bytes are not valid UTF-8")]
    InvalidUtf8,
}

/// Decoded form of a `KESTRELFS_OP_GETATTR` request payload. See
/// [`KestrelfsEvent::decode_getattr_req`].
#[derive(Debug, Clone, Copy)]
pub struct GetattrReq {
    pub inode_id: u64,
}

/// Attribute fields carried by a successful `KESTRELFS_OP_LOOKUP` or
/// `KESTRELFS_OP_GETATTR` response.
///
/// Deliberately a small, flat, `Copy` struct of exactly the
/// wire-relevant fields - NOT `crate::fs_model::Inode` passed by
/// reference. This keeps `abi.rs` fully decoupled from `fs_model.rs`:
/// this module's whole purpose is to mirror `kestrelfs_ipc.h`'s wire
/// format byte-for-byte (see the module doc comment), and the wire
/// format is deliberately a *different, smaller* shape than `Inode`
/// (no separate atime/ctime, no `inode_id` self-reference in the
/// GETATTR response - see that response's doc comment above). Letting
/// `fs_model::Inode` grow additional fields later (e.g. extended
/// attributes) must never risk an accidental "just serialize the
/// whole struct" shortcut here - every wire field is assembled
/// explicitly by whichever caller (see `main.rs`) already holds both
/// an `Inode` and, where relevant, a freshly resolved child inode id.
#[derive(Debug, Clone, Copy)]
pub struct AttrFields {
    pub size: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub nlink: u32,
    pub mtime: u64,
}

/// Decoded form of a `KESTRELFS_OP_READ_CHUNK` request payload. See
/// [`KestrelfsEvent::decode_read_chunk_req`].
#[derive(Debug, Clone, Copy)]
pub struct ReadChunkReq {
    pub inode_id: u64,
    pub offset: u64,
    pub count: u32,
}

/// Decoded `KESTRELFS_OP_WRITE_CHUNK` request payload.
///
/// Layout (ABI v3):
/// - `inode_id` at offset 0 (u64)
/// - `offset` at offset 8 (u64)
/// - `count` at offset 16 (u32, max 12)
/// - `data` at offset 20 (up to 12 bytes)
pub struct WriteChunkReq {
    pub inode_id: u64,
    pub offset: u64,
    pub count: u32,
    pub data: Vec<u8>,
}

/// Decoded ABI v8 bulk data request shared by READ_DATA and WRITE_DATA.
#[derive(Debug, Clone, Copy)]
pub struct DataReq {
    pub inode_id: u64,
    pub offset: u64,
    pub length: u32,
}

/// Decoded KESTRELFS_OP_TRUNCATE request.
#[derive(Debug, Clone)]
pub struct TruncateReq {
    pub inode_id: u64,
    pub new_size: u64,
}

/// Decoded `KESTRELFS_OP_CREATE` request payload.
pub struct CreateReq {
    pub parent_inode: u64,
    pub mode: u32,
    pub name: String,
}

/// Decoded common ABI v10 single-name DATA request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameDataReq {
    pub parent_inode: u64,
    pub mode: u32,
    pub name: String,
}

/// Decoded ABI v12 `OP_LINK_DATA` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkDataReq {
    pub parent_inode: u64,
    pub inode_id: u64,
    pub name: String,
}

/// Decoded ABI v11 symbolic-link creation request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymlinkDataReq {
    pub parent_inode: u64,
    pub name: String,
    pub target: String,
}

/// Malformed ABI v11 `OP_SYMLINK_DATA` request.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SymlinkDataDecodeError {
    #[error("request flags {0:#x} are unsupported")]
    UnsupportedFlags(u32),
    #[error("link name must not be empty")]
    EmptyName,
    #[error("name_len {0} exceeds NAME_DATA_MAX ({NAME_DATA_MAX})")]
    NameTooLong(u16),
    #[error("symlink target must not be empty")]
    EmptyTarget,
    #[error("target_len {0} exceeds SYMLINK_TARGET_MAX ({SYMLINK_TARGET_MAX})")]
    TargetTooLong(u16),
    #[error("combined symlink name and target exceed DATA_BUFFER_SIZE")]
    CombinedDataTooLong,
    #[error("link name is not valid UTF-8")]
    NameInvalidUtf8,
    #[error("symlink target is not valid UTF-8")]
    TargetInvalidUtf8,
}

/// Malformed ABI v10 single-name DATA request.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NameDataDecodeError {
    #[error("request flags {0:#x} are unsupported")]
    UnsupportedFlags(u32),
    #[error("name must not be empty")]
    EmptyName,
    #[error("name_len {0} exceeds NAME_DATA_MAX ({NAME_DATA_MAX})")]
    NameTooLong(u16),
    #[error("name is not valid UTF-8")]
    InvalidUtf8,
}

/// Decoded `KESTRELFS_OP_READDIR` request payload.
pub struct ReaddirReq {
    pub dir_inode: u64,
    pub offset: u32,
}

/// Decoded `KESTRELFS_OP_RENAME` request payload.
#[derive(Debug, Clone)]
pub struct RenameReq {
    pub old_parent: u64,
    pub new_parent: u64,
    pub old_name: String,
    pub new_name: String,
    pub flags: u32,
    pub defer_reclaim: bool,
}

/// Errors [`KestrelfsEvent::decode_rename_req`] can report.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RenameDecodeError {
    /// Old name length exceeds [`RENAME_NAME_MAX`].
    #[error("old_name_len {0} exceeds RENAME_NAME_MAX ({RENAME_NAME_MAX})")]
    OldNameTooLong(u8),
    /// New name length exceeds [`RENAME_NAME_MAX`].
    #[error("new_name_len {0} exceeds RENAME_NAME_MAX ({RENAME_NAME_MAX})")]
    NewNameTooLong(u8),
    /// Invalid UTF-8 in old_name.
    #[error("invalid UTF-8 in old_name")]
    OldNameInvalidUtf8,
    /// Invalid UTF-8 in new_name.
    #[error("invalid UTF-8 in new_name")]
    NewNameInvalidUtf8,
}

/// Errors [`KestrelfsEvent::decode_rename_data_req`] can report.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RenameDataDecodeError {
    #[error("request flags {0:#x} are unsupported")]
    UnsupportedFlags(u32),
    #[error("rename flags {0:#x} are unsupported")]
    UnsupportedRenameFlags(u32),
    #[error("lifecycle flags {0:#x} are unsupported")]
    UnsupportedLifecycleFlags(u32),
    #[error(
        "old_name_len {0} exceeds RENAME_DATA_NAME_MAX ({RENAME_DATA_NAME_MAX})"
    )]
    OldNameTooLong(u16),
    #[error(
        "new_name_len {0} exceeds RENAME_DATA_NAME_MAX ({RENAME_DATA_NAME_MAX})"
    )]
    NewNameTooLong(u16),
    #[error("combined rename names exceed DATA_BUFFER_SIZE ({DATA_BUFFER_SIZE})")]
    CombinedNamesTooLong,
    #[error("invalid UTF-8 in old_name")]
    OldNameInvalidUtf8,
    #[error("invalid UTF-8 in new_name")]
    NewNameInvalidUtf8,
}

// ---------------------------------------------------------------------
// struct kestrelfs_ring_ctrl
// ---------------------------------------------------------------------

/// Mirrors `struct kestrelfs_ring_ctrl` in `kestrelfs_ipc.h`.
///
/// `head` and `tail` are `AtomicU64` here (NOT plain `u64`) because,
/// unlike [`KestrelfsEvent`], this struct's fields are the actual
/// cross-process synchronization primitives - every access MUST go
/// through an explicit `Ordering`, exactly as the C side must use
/// `smp_store_release()`/`smp_load_acquire()` rather than plain reads
/// and writes. Using `AtomicU64::load(Ordering::Relaxed)` by accident
/// instead of `Ordering::Acquire` would be exactly as dangerous as the C
/// side accidentally using `READ_ONCE()` where `smp_load_acquire()` was
/// required - the type system here does not prevent choosing the wrong
/// `Ordering`, only prevents forgetting to go through an atomic op at
/// all (which prevents the much more common bug of a plain data race
/// under the Rust/LLVM memory model, and matches what the linked
/// `_pad` comment in the C header already documents as a requirement).
///
/// `AtomicU64` has the same size and (on x86_64) the same alignment as
/// a plain `u64`, so this substitution does not change the struct's
/// layout - verified by [`compile_time_layout_asserts`] below.
#[repr(C)]
pub struct KestrelfsRingCtrl {
    pub head: AtomicU64,
    pub tail: AtomicU64,
    pub capacity: AtomicU32,
    pub _reserved0: u32,
    pub _pad: [u8; CACHELINE_SIZE - (2 * 8 + 2 * 4)],
}

// ---------------------------------------------------------------------
// struct kestrelfs_shared_region
// ---------------------------------------------------------------------

/// Mirrors `struct kestrelfs_shared_region` in `kestrelfs_ipc.h` - the
/// entire `mmap()`-ed layout.
///
/// # Safety contract for callers
///
/// A `*mut KestrelfsSharedRegion` obtained from `mmap()` points at
/// kernel-owned memory that is concurrently read/written by the kernel
/// module. Every access to `req_slots`/`resp_slots` element contents
/// (as opposed to the atomic `head`/`tail` control fields) must be
/// preceded by an acquire load of the relevant `head` that establishes
/// "this slot index is safely published" - see [`crate::ring`] for the
/// actual push/pop protocol built on top of this type. This struct
/// itself only defines layout; it enforces no ordering by construction
/// (Rust's type system has no way to express "this field may only be
/// read after that atomic load", so the discipline is documented here
/// and enforced by code review / the `ring` module's API surface, not
/// by the compiler).
#[repr(C)]
pub struct KestrelfsSharedRegion {
    pub magic: u32,
    pub abi_version: u32,
    pub _pad0: [u8; CACHELINE_SIZE - 2 * 4],

    pub req_ctrl: KestrelfsRingCtrl,
    pub resp_ctrl: KestrelfsRingCtrl,

    pub req_slots: [KestrelfsEvent; RING_SLOTS],
    pub resp_slots: [KestrelfsEvent; RING_SLOTS],
    pub data_buffer: [u8; DATA_BUFFER_SIZE],
}

/// Mirrors `KESTRELFS_SHM_REGION_SIZE` (`sizeof(struct
/// kestrelfs_shared_region)` on the C side). Used to size the `mmap()`
/// call and cross-checked against the kernel's own report of this value
/// via `KESTRELFS_IOC_GET_REGION_SIZE` before trusting the mapping at
/// all - see [`super::device::KestrelDevice::open`].
pub const SHM_REGION_SIZE: usize = std::mem::size_of::<KestrelfsSharedRegion>();

// True compile-time counterparts to the C `_Static_assert`s. Array lengths
// must match exactly or rustc rejects the ABI mirror.
const _: [(); 147_648] = [(); SHM_REGION_SIZE];
const _: [(); 131_264] = [(); std::mem::offset_of!(
    KestrelfsSharedRegion,
    data_buffer
)];
const _: () = assert!(8 + 8 + 4 <= EVENT_PAYLOAD_SIZE);
const _: () = assert!(8 + 8 + 2 + 2 + 4 + 4 <= EVENT_PAYLOAD_SIZE);
const _: () = assert!(8 <= EVENT_PAYLOAD_SIZE);
const _: () = assert!(2 * RENAME_DATA_NAME_MAX <= DATA_BUFFER_SIZE);
const _: () = assert!(8 + 2 + 2 + 4 <= EVENT_PAYLOAD_SIZE);
const _: () = assert!(NAME_DATA_MAX <= u16::MAX as usize);
const _: () = assert!(NAME_DATA_MAX == RENAME_DATA_NAME_MAX);
const _: () =
    assert!(READDIR_DATA_ENTRY_HEADER_SIZE + NAME_DATA_MAX <= DATA_BUFFER_SIZE);
const _: () = assert!(8 + 2 + 2 <= EVENT_PAYLOAD_SIZE);
const _: () = assert!(NAME_DATA_MAX + SYMLINK_TARGET_MAX <= DATA_BUFFER_SIZE);
const _: () = assert!(SYMLINK_TARGET_MAX <= u16::MAX as usize);
const _: () = assert!(8 + 8 + 2 <= EVENT_PAYLOAD_SIZE);

/// Compile-time layout assertions, mirroring the `_Static_assert`s at
/// the bottom of `kestrelfs_ipc.h`. Called once from `main()` - Rust
/// has no direct `static_assert` construct as clean as C11's outside of
/// `const` evaluation contexts, so we assert on plain `const` values
/// computed via `size_of`/`align_of` instead. Because every value here
/// is a `const`, a failing assertion is caught the moment this function
/// is exercised (we call it unconditionally at the top of `main()`), not
/// buried behind a rarely-hit runtime code path.
pub fn compile_time_layout_asserts() {
    assert_eq!(
        std::mem::size_of::<KestrelfsEvent>(),
        64,
        "KestrelfsEvent must be exactly 64 bytes (one cacheline), matches \
         the _Static_assert in kestrelfs_ipc.h"
    );

    assert_eq!(
        std::mem::size_of::<KestrelfsRingCtrl>(),
        CACHELINE_SIZE,
        "KestrelfsRingCtrl must be exactly one cacheline"
    );
    assert_eq!(
        std::mem::align_of::<KestrelfsRingCtrl>(),
        std::mem::align_of::<u64>(),
        "KestrelfsRingCtrl alignment must match plain u64 alignment - if \
         this ever fails, AtomicU64 stopped being layout-compatible with \
         u64 on this target and the whole ABI mirror is unsound"
    );

    assert_eq!(
        SHM_REGION_SIZE, 147_648,
        "kestrelfs_shared_region size drifted from the value verified \
         against the running kernel module during Phase 2 development \
         (see README/design notes); if this legitimately changed, the \
         C header, KESTRELFS_ABI_VERSION, and this constant must all be \
         bumped together"
    );

    assert_eq!(
        std::mem::offset_of!(KestrelfsSharedRegion, data_buffer),
        131_264,
        "data_buffer must immediately follow both event rings"
    );

    assert_eq!(
        RING_SLOTS & (RING_SLOTS - 1),
        0,
        "RING_SLOTS must be a power of two"
    );
}

// NOTE(bindgen): a future iteration of this module can be replaced by
// running `bindgen kestrelfs/kestrelfs_ipc.h -o daemon/src/abi_generated.rs`
// from a `build.rs`, which removes the hand-mirroring risk entirely by
// generating these types directly from the canonical C header. This was
// deliberately deferred for this Phase 2 step to keep the diff reviewable
// field-by-field against kestrelfs_ipc.h; the `compile_time_layout_asserts`
// checks above are what stand in for that guarantee until then.

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_attrs() -> AttrFields {
        AttrFields {
            size: 512,
            mode: 0o100644,
            uid: 1000,
            gid: 1000,
            nlink: 1,
            mtime: 1_700_000_000,
        }
    }

    // --- KESTRELFS_OP_LOOKUP request encode/decode -----------------

    /// Builds a raw LOOKUP request payload by hand (bypassing any
    /// encoder), matching kestrelfs_ipc.h's documented byte layout
    /// exactly - this is what pins the *wire offsets themselves*,
    /// independent of whatever encoding helper this module provides,
    /// so a bug in a future encoder can never accidentally validate
    /// itself against its own (possibly also buggy) inverse.
    fn raw_lookup_req(parent_inode: u64, name: &[u8]) -> KestrelfsEvent {
        let mut event = KestrelfsEvent::zeroed(OP_LOOKUP, 42);
        event.payload[0..8].copy_from_slice(&parent_inode.to_le_bytes());
        event.payload[8] = name.len() as u8;
        event.payload[9..9 + name.len()].copy_from_slice(name);
        event
    }

    #[test]
    fn decode_lookup_req_reads_parent_inode_at_offset_0() {
        let event = raw_lookup_req(0x0102_0304_0506_0708, b"x");
        let decoded = event.decode_lookup_req().expect("valid payload");
        assert_eq!(decoded.parent_inode, 0x0102_0304_0506_0708);
    }

    #[test]
    fn decode_lookup_req_reads_name_len_at_offset_8_and_name_at_offset_9() {
        let event = raw_lookup_req(1, b"remote.txt");
        let decoded = event.decode_lookup_req().expect("valid payload");
        assert_eq!(decoded.name, "remote.txt");
    }

    #[test]
    fn decode_lookup_req_accepts_max_length_name() {
        let name = vec![b'a'; LOOKUP_NAME_MAX];
        let event = raw_lookup_req(1, &name);
        let decoded = event
            .decode_lookup_req()
            .expect("max-length name must be accepted");
        assert_eq!(decoded.name.len(), LOOKUP_NAME_MAX);
    }

    #[test]
    fn decode_lookup_req_rejects_name_len_over_max() {
        // Hand-craft a payload with an out-of-spec name_len byte,
        // rather than going through raw_lookup_req (which cannot
        // itself express an invalid length without also overflowing
        // the 32-byte payload array in the test helper itself).
        let mut event = KestrelfsEvent::zeroed(OP_LOOKUP, 1);
        event.payload[8] = (LOOKUP_NAME_MAX + 1) as u8;
        let err = event.decode_lookup_req().unwrap_err();
        assert_eq!(
            err,
            LookupDecodeError::NameTooLong((LOOKUP_NAME_MAX + 1) as u8)
        );
    }

    #[test]
    fn decode_lookup_req_rejects_invalid_utf8() {
        let invalid_utf8 = [0xFFu8, 0xFE, 0xFD];
        let event = raw_lookup_req(1, &invalid_utf8);
        let err = event.decode_lookup_req().unwrap_err();
        assert_eq!(err, LookupDecodeError::InvalidUtf8);
    }

    #[test]
    fn decode_lookup_req_empty_name() {
        let event = raw_lookup_req(1, b"");
        let decoded = event
            .decode_lookup_req()
            .expect("empty name is a valid (if unusual) payload");
        assert_eq!(decoded.name, "");
    }

    // --- KESTRELFS_OP_LOOKUP response encode ------------------------

    #[test]
    fn lookup_response_uses_every_payload_byte_at_documented_offsets() {
        let event = KestrelfsEvent::lookup_response(7, 2, sample_attrs());

        assert_eq!(event.opcode, OP_RESULT_OK);
        assert_eq!(event.req_id, 7);
        assert_eq!(
            u64::from_le_bytes(event.payload[0..8].try_into().unwrap()),
            2
        );
        assert_eq!(
            u64::from_le_bytes(event.payload[8..16].try_into().unwrap()),
            512
        );
        assert_eq!(
            u32::from_le_bytes(event.payload[16..20].try_into().unwrap()),
            0o100644
        );
        assert_eq!(
            u32::from_le_bytes(event.payload[20..24].try_into().unwrap()),
            1000
        );
        assert_eq!(
            u32::from_le_bytes(event.payload[24..28].try_into().unwrap()),
            1000
        );
        assert_eq!(
            u32::from_le_bytes(event.payload[28..32].try_into().unwrap()),
            1
        );
    }

    // --- KESTRELFS_OP_GETATTR request decode ------------------------

    #[test]
    fn decode_getattr_req_reads_inode_id_at_offset_0() {
        let mut event = KestrelfsEvent::zeroed(OP_GETATTR, 1);
        event.payload[0..8].copy_from_slice(&99u64.to_le_bytes());
        let decoded = event.decode_getattr_req();
        assert_eq!(decoded.inode_id, 99);
    }

    // --- KESTRELFS_OP_GETATTR response encode -----------------------

    #[test]
    fn getattr_response_uses_every_payload_byte_at_documented_offsets() {
        let event = KestrelfsEvent::getattr_response(9, sample_attrs());

        assert_eq!(event.opcode, OP_RESULT_OK);
        assert_eq!(event.req_id, 9);
        assert_eq!(
            u64::from_le_bytes(event.payload[0..8].try_into().unwrap()),
            512
        );
        assert_eq!(
            u32::from_le_bytes(event.payload[8..12].try_into().unwrap()),
            0o100644
        );
        assert_eq!(
            u32::from_le_bytes(event.payload[12..16].try_into().unwrap()),
            1000
        );
        assert_eq!(
            u32::from_le_bytes(event.payload[16..20].try_into().unwrap()),
            1000
        );
        assert_eq!(
            u32::from_le_bytes(event.payload[20..24].try_into().unwrap()),
            1
        );
        assert_eq!(
            u64::from_le_bytes(event.payload[24..32].try_into().unwrap()),
            1_700_000_000
        );
    }

    #[test]
    fn getattr_response_does_not_repeat_an_inode_id() {
        // Deliberate contrast with lookup_response: GETATTR's wire
        // format spends its first 8 bytes on `size`, not an inode id
        // - see kestrelfs_ipc.h's doc comment on why. Byte 0 must NOT
        // equal the requested inode_id unless size happens to be
        // equal by coincidence (it isn't, in this fixture).
        let attrs = AttrFields {
            size: 4096,
            ..sample_attrs()
        };
        let event = KestrelfsEvent::getattr_response(1, attrs);
        let first_field = u64::from_le_bytes(event.payload[0..8].try_into().unwrap());
        assert_eq!(first_field, 4096);
        assert_ne!(first_field, 2 /* some unrelated inode id */);
    }

    // --- Wire budget sanity (mirrors the C header's _Static_asserts) ---

    #[test]
    fn lookup_name_max_fits_request_payload_budget() {
        // parent_inode(8) + name_len(1) + name(LOOKUP_NAME_MAX) must
        // fit within EVENT_PAYLOAD_SIZE - mirrors the C header's
        // _Static_assert of the same arithmetic. Wrapped in a `const`
        // block (rather than a plain runtime `assert!`) since clippy
        // correctly observes every operand here is already a
        // compile-time constant - evaluating it at const-eval time
        // is both more efficient and fails the build immediately if
        // it were ever wrong, rather than only when this test runs.
        const { assert!(8 + 1 + LOOKUP_NAME_MAX <= EVENT_PAYLOAD_SIZE) };
    }

    #[test]
    fn lookup_response_fields_sum_to_full_payload() {
        const { assert!(8 + 8 + 4 + 4 + 4 + 4 == EVENT_PAYLOAD_SIZE) };
    }

    #[test]
    fn getattr_response_fields_sum_to_full_payload() {
        const { assert!(8 + 4 + 4 + 4 + 4 + 8 == EVENT_PAYLOAD_SIZE) };
    }

    #[test]
    fn decode_write_chunk_req_extracts_all_fields() {
        let mut event = KestrelfsEvent {
            seq: 0,
            opcode: OP_WRITE_CHUNK,
            flags: 0,
            req_id: 123,
            error_code: 0,
            _reserved0: 0,
            payload: [0u8; EVENT_PAYLOAD_SIZE],
        };

        // inode_id = 42 at offset 0
        event.payload[0..8].copy_from_slice(&42u64.to_le_bytes());
        // offset = 1000 at offset 8
        event.payload[8..16].copy_from_slice(&1000u64.to_le_bytes());
        // count = 5 at offset 16
        event.payload[16..20].copy_from_slice(&5u32.to_le_bytes());
        // data = "hello" at offset 20
        event.payload[20..25].copy_from_slice(b"hello");

        let req = event.decode_write_chunk_req();
        assert_eq!(req.inode_id, 42);
        assert_eq!(req.offset, 1000);
        assert_eq!(req.count, 5);
        assert_eq!(req.data, b"hello");
    }

    #[test]
    fn decode_write_chunk_req_clamps_count_to_max_12() {
        let mut event = KestrelfsEvent {
            seq: 0,
            opcode: OP_WRITE_CHUNK,
            flags: 0,
            req_id: 1,
            error_code: 0,
            _reserved0: 0,
            payload: [0u8; EVENT_PAYLOAD_SIZE],
        };

        // count = 999 (invalid, should be clamped to 12)
        event.payload[16..20].copy_from_slice(&999u32.to_le_bytes());
        event.payload[20..32].copy_from_slice(b"123456789012");

        let req = event.decode_write_chunk_req();
        assert_eq!(req.count, 12);
        assert_eq!(req.data.len(), 12);
        assert_eq!(&req.data, b"123456789012");
    }

    #[test]
    fn write_chunk_request_fits_payload_budget() {
        // inode_id(8) + offset(8) + count(4) + data(12) = 32
        const { assert!(8 + 8 + 4 + 12 == EVENT_PAYLOAD_SIZE) };
    }

    #[test]
    fn decode_truncate_req_works() {
        let mut event = KestrelfsEvent::zeroed(OP_TRUNCATE, 123);
        event.payload[0..8].copy_from_slice(&42u64.to_le_bytes());
        event.payload[8..16].copy_from_slice(&1024u64.to_le_bytes());

        let req = event.decode_truncate_req();
        assert_eq!(req.inode_id, 42);
        assert_eq!(req.new_size, 1024);
    }

    #[test]
    fn truncate_request_fits_payload_budget() {
        // inode_id(8) + new_size(8) = 16, well within 32 bytes
        const { assert!(8 + 8 <= EVENT_PAYLOAD_SIZE) };
    }

    #[test]
    fn decode_link_data_req_reads_fixed_fields_and_bounce_name() {
        let name = "long-hard-link-name-over-twenty-three-bytes";
        let mut data = [0u8; DATA_BUFFER_SIZE];
        data[..name.len()].copy_from_slice(name.as_bytes());
        let mut event = KestrelfsEvent::zeroed(OP_LINK_DATA, 77);
        event.payload[0..8].copy_from_slice(&41u64.to_le_bytes());
        event.payload[8..16].copy_from_slice(&99u64.to_le_bytes());
        event.payload[16..18].copy_from_slice(&(name.len() as u16).to_le_bytes());

        // SAFETY: data is a live DATA_BUFFER_SIZE allocation owned by the test.
        let decoded = unsafe { event.decode_link_data_req(data.as_ptr()) }.unwrap();
        assert_eq!(
            decoded,
            LinkDataReq {
                parent_inode: 41,
                inode_id: 99,
                name: name.to_owned(),
            }
        );
    }

    #[test]
    fn decode_rename_data_req_reads_payload_flags_at_offset_20() {
        let mut data = [0u8; DATA_BUFFER_SIZE];
        data[..6].copy_from_slice(b"oldnew");
        let mut event = KestrelfsEvent::zeroed(OP_RENAME_DATA, 78);
        event.payload[0..8].copy_from_slice(&41u64.to_le_bytes());
        event.payload[8..16].copy_from_slice(&42u64.to_le_bytes());
        event.payload[16..18].copy_from_slice(&3u16.to_le_bytes());
        event.payload[18..20].copy_from_slice(&3u16.to_le_bytes());
        event.payload[20..24].copy_from_slice(&RENAME_NOREPLACE.to_le_bytes());

        // SAFETY: data is a live DATA_BUFFER_SIZE allocation owned by the test.
        let decoded = unsafe { event.decode_rename_data_req(data.as_ptr()) }.unwrap();
        assert_eq!(decoded.old_parent, 41);
        assert_eq!(decoded.new_parent, 42);
        assert_eq!(decoded.old_name, "old");
        assert_eq!(decoded.new_name, "new");
        assert_eq!(decoded.flags, RENAME_NOREPLACE);
        assert!(!decoded.defer_reclaim);
    }

    #[test]
    fn decode_rename_data_req_reads_lifecycle_flags_at_offset_24() {
        let mut data = [0u8; DATA_BUFFER_SIZE];
        data[..6].copy_from_slice(b"oldnew");
        let mut event = KestrelfsEvent::zeroed(OP_RENAME_DATA, 80);
        event.payload[16..18].copy_from_slice(&3u16.to_le_bytes());
        event.payload[18..20].copy_from_slice(&3u16.to_le_bytes());
        event.payload[24..28].copy_from_slice(&LIFECYCLE_DEFER_RECLAIM.to_le_bytes());

        // SAFETY: data is a live DATA_BUFFER_SIZE allocation owned by the test.
        let decoded = unsafe { event.decode_rename_data_req(data.as_ptr()) }.unwrap();
        assert!(decoded.defer_reclaim);
    }

    #[test]
    fn decode_rename_data_req_rejects_unknown_payload_flags() {
        let data = [0u8; DATA_BUFFER_SIZE];
        let mut event = KestrelfsEvent::zeroed(OP_RENAME_DATA, 79);
        event.payload[20..24].copy_from_slice(&2u32.to_le_bytes());

        // SAFETY: data is a live DATA_BUFFER_SIZE allocation owned by the test.
        let error = unsafe { event.decode_rename_data_req(data.as_ptr()) }.unwrap_err();
        assert_eq!(error, RenameDataDecodeError::UnsupportedRenameFlags(2));
    }
}
