// SPDX-License-Identifier: GPL-2.0 WITH Linux-syscall-note
//! KestrelFS control-plane daemon - Phase 2 bootstrap.
//!
//! This binary is **not yet the real control-plane daemon** described
//! in the project roadmap (no Redis, no S3, no Tokio). It exists solely
//! to prove the kernel<->Rust IPC bridge built in Phase 2 works
//! end-to-end: open `/dev/kestrel_ctl`, validate the ABI, `mmap()` the
//! shared ring-buffer region, then run a `poll()`-driven loop that
//! drains REQ events pushed by the kernel (via the `selftest_push`
//! debugfs trigger, see `kestrelfs/ipc_ring.c`) and echoes back a
//! matching RESP event for each one.
//!
//! # Manual end-to-end test
//!
//! With the `kestrelfs.ko` module loaded (see the project README):
//!
//! ```text
//! # terminal 1
//! sudo ./target/debug/kestrelfs-daemon
//!
//! # terminal 2, any number of times
//! sudo sh -c 'echo 1 > /sys/kernel/debug/kestrelfs/selftest_push'
//! ```
//!
//! Terminal 1 should print one line per pushed request, and `dmesg`
//! should show `kestrelfs_check_resp` succeeding (instead of the
//! `-ENOENT` seen in Phase 2 step 3, before this daemon existed to
//! answer).

mod abi;
mod device;
mod ioctl;
mod ring;

use abi::KestrelfsEvent;
use device::KestrelDevice;
use std::io;

fn main() -> io::Result<()> {
    abi::compile_time_layout_asserts();

    println!("kestrelfs-daemon: opening /dev/kestrel_ctl ...");
    let dev = KestrelDevice::open()?;
    println!(
        "kestrelfs-daemon: attached OK (abi_version={}, region_size={} bytes)",
        abi::ABI_VERSION,
        abi::SHM_REGION_SIZE
    );
    println!("kestrelfs-daemon: entering poll() event loop, waiting for REQ events ...");

    event_loop(&dev)
}

/// Blocks in `poll()` on the device fd until the kernel wakes us up
/// (via `wake_up_interruptible()` in `kestrelfs_req_push()`), then
/// drains and answers every pending REQ event, forever.
///
/// This is deliberately a simple, single-threaded loop for this
/// bootstrap step - see the module doc comment on `ring.rs` for why
/// that is sufficient for the current single-producer/single-consumer
/// role split.
fn event_loop(dev: &KestrelDevice) -> io::Result<()> {
    let mut pfd = libc::pollfd {
        fd: dev.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };

    loop {
        pfd.revents = 0;

        // SAFETY: `&mut pfd` points at a single valid `libc::pollfd`
        // on the stack, matching the `nfds = 1` argument. `-1` as the
        // timeout requests an indefinite block, matching this
        // function's documented "wait forever" behavior. `poll()`
        // itself performs no memory access beyond reading/writing
        // through this one pointer, which is safe C-ABI FFI as long
        // as the pointer and count agree, which they do here.
        let ret = unsafe { libc::poll(&mut pfd as *mut libc::pollfd, 1, -1) };

        if ret < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                // A signal (e.g. during interactive Ctrl-C testing)
                // interrupted the syscall; retry rather than treating
                // this as a fatal error.
                continue;
            }
            return Err(err);
        }

        if pfd.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
            return Err(io::Error::other(format!(
                "kestrelfs-daemon: /dev/kestrel_ctl fd reported POLLERR/POLLNVAL (revents=0x{:x})",
                pfd.revents
            )));
        }

        if pfd.revents & libc::POLLIN == 0 {
            // Spurious wakeup (or POLLHUP with nothing readable);
            // nothing to drain, go back to sleep.
            continue;
        }

        drain_and_respond(dev);
    }
}

/// Drains every currently-pending REQ event and pushes back one
/// matching RESP event per request, then notifies the kernel once via
/// `KESTRELFS_IOC_NOTIFY_RESP` if at least one response was published.
///
/// Batching the single `notify_resp()` call after the whole drain
/// (rather than one ioctl per event) mirrors how the kernel's own
/// `kestrelfs_wake_req_waiters()` is called once per
/// `kestrelfs_req_push()` rather than per-slot - both sides favor
/// "notify once after doing a batch of ring work" over "notify on
/// every single slot", trading a small amount of response latency for
/// far fewer syscalls under load.
fn drain_and_respond(dev: &KestrelDevice) {
    // SAFETY: `dev.region_ptr()` returns the pointer established by
    // `KestrelDevice::open()`'s successful `mmap()`, which remains
    // valid for as long as `dev` (borrowed here) is alive - guaranteed
    // by this function's `&KestrelDevice` parameter outliving the
    // call. This is the daemon's single consumer thread for the REQ
    // ring (see `ring.rs` module doc comment), satisfying
    // `drain_requests`'s "no concurrent caller" safety requirement.
    let mut responded = 0u64;
    let drained = unsafe {
        ring::drain_requests(dev.region_ptr(), |event: &KestrelfsEvent| {
            println!(
                "kestrelfs-daemon: <- REQ  seq={} req_id={} opcode={} flags={}",
                event.seq, event.req_id, event.opcode, event.flags
            );

            let response = KestrelfsEvent::zeroed(abi::OP_RESULT_OK, event.req_id);

            // SAFETY: same reasoning as the `drain_requests` call
            // below - `dev.region_ptr()` is valid for the duration of
            // this call, and this daemon is the RESP ring's sole
            // producer thread, satisfying `push_response`'s safety
            // requirement. This closure body is already lexically
            // inside the `unsafe` block wrapping `drain_requests`
            // below, so no additional `unsafe { }` is needed (and
            // rustc rightly warns if one is added).
            let pushed = ring::push_response(dev.region_ptr(), response);

            if pushed {
                responded += 1;
                println!(
                    "kestrelfs-daemon: -> RESP req_id={} opcode=RESULT_OK",
                    event.req_id
                );
            } else {
                eprintln!(
                    "kestrelfs-daemon: RESP ring full, dropping response for req_id={}",
                    event.req_id
                );
            }
        })
    };

    if drained > 0 {
        println!("kestrelfs-daemon: drained {drained} REQ event(s)");
    }

    if responded > 0 {
        if let Err(e) = dev.notify_resp() {
            eprintln!("kestrelfs-daemon: KESTRELFS_IOC_NOTIFY_RESP failed: {e}");
        }
    }
}
