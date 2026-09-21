// SPDX-License-Identifier: GPL-2.0 WITH Linux-syscall-note
//! KestrelFS control-plane daemon.
//!
//! # Status
//!
//! This binary is an early control-plane daemon: it now has local and Redis
//! metadata backends plus local and S3-compatible object backends, while the
//! Phase 4 data plane remains unimplemented.
//! Its original bootstrap established two things
//! end-to-end:
//!
//! 1. (Phase 2) The kernel<->Rust IPC bridge itself: open
//!    `/dev/kestrel_ctl`, validate the ABI, `mmap()` the shared
//!    ring-buffer region, then run a `poll()`-driven loop that drains
//!    REQ events pushed by the kernel and pushes back a matching RESP
//!    event for each one.
//! 2. (Phase 3 step 2) That REQ events can be answered from a real
//!    (if in-memory) metadata store rather than synthesized data:
//!    `KESTRELFS_OP_LOOKUP`/`KESTRELFS_OP_GETATTR` requests are now
//!    answered by querying a [`meta::MemStore`] - see
//!    [`build_response`]. `KESTRELFS_OP_READ_CHUNK` deliberately still
//!    answers with the same synthetic, deterministic string it always
//!    has (see [`handle_read_chunk`]) - wiring it to
//!    `MetaStore::read_slices`/real block storage is out of scope for
//!    this step (see the Phase 3 step 2 task description).
//!
//! # Why a Tokio runtime for an otherwise-synchronous poll() loop
//!
//! [`meta::MetaStore`]'s methods are `async fn` (see that trait's doc
//! comment for why: the eventual production backing store is a
//! network-attached Redis, so every method must be async from day
//! one). The IPC event loop itself, however, is fundamentally
//! synchronous: `libc::poll()` is a blocking syscall with no async
//! equivalent in play here (there is deliberately no async I/O
//! runtime integration for the character device fd itself - see
//! `ring.rs`'s module doc comment on the single-producer/single-consumer
//! design). Rather than restructure the whole event loop around an
//! async runtime's I/O reactor (unnecessary complexity for a
//! `MemStore` that never actually awaits on real I/O), this daemon
//! keeps `main()`/`event_loop()` fully synchronous and creates one
//! [`tokio::runtime::Runtime`], calling [`tokio::runtime::Runtime::block_on`]
//! only at the single call site (inside [`build_response`]) that
//! needs to invoke an `async fn`. This preserves the event loop's
//! existing single-consumer/single-producer ring invariant exactly
//! (see `ring.rs`) - `drain_requests`/`push_response` are still only
//! ever called from this one synchronous thread, `block_on` merely
//! drives a `Future` to completion inline before returning control to
//! that same thread, it does not hand any ring access off to a
//! different runtime-managed thread.

mod abi;
mod device;
mod fs_model;
mod gc_worker;
mod ioctl;
mod meta;
mod meta_persist;
mod meta_redis;
mod object_store;
mod object_store_s3;
mod ring;

// Keep the storage model's validation limit pinned to the wire contract.
const _: () = assert!(abi::SYMLINK_TARGET_MAX == fs_model::SYMLINK_TARGET_MAX);

use abi::{AttrFields, KestrelfsEvent};
use clap::Parser;
use device::KestrelDevice;
use fs_model::{Inode, Slice};
use gc_worker::{DeleteCompletion, GcScheduler, GcWorker};
use meta::{CoherenceProbe, MetaError, MetaStore};
use object_store::ObjectStore;
use std::collections::HashSet;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

const GC_RETRY_BASE: Duration = Duration::from_secs(1);
const GC_RETRY_MAX: Duration = Duration::from_secs(60);
const COHERENCE_POLL_INTERVAL: Duration = Duration::from_millis(100);
const ORPHAN_SWEEP_INTERVAL: Duration = Duration::from_secs(1);
static GC_PASSES: AtomicU64 = AtomicU64::new(0);
static GC_ATTEMPTED: AtomicU64 = AtomicU64::new(0);
static GC_DELETED: AtomicU64 = AtomicU64::new(0);
static GC_FAILURES: AtomicU64 = AtomicU64::new(0);
static GC_SCHEDULER: OnceLock<GcScheduler> = OnceLock::new();

fn next_gc_retry_delay(current: Duration, retry_needed: bool) -> Duration {
    if retry_needed {
        current.saturating_mul(2).min(GC_RETRY_MAX)
    } else {
        GC_RETRY_BASE
    }
}

#[derive(Debug, PartialEq, Eq)]
enum CoherenceInvalidation {
    None,
    Inodes(Vec<u64>),
    All,
}

fn coherence_invalidation(probe: Option<&CoherenceProbe>) -> CoherenceInvalidation {
    match probe {
        Some(CoherenceProbe::Inodes { inode_ids, .. }) if !inode_ids.is_empty() => {
            CoherenceInvalidation::Inodes(inode_ids.clone())
        }
        Some(CoherenceProbe::Full { .. }) | None => CoherenceInvalidation::All,
        Some(CoherenceProbe::Disabled)
        | Some(CoherenceProbe::Unchanged { .. })
        | Some(CoherenceProbe::Inodes { .. }) => CoherenceInvalidation::None,
    }
}

/// KestrelFS control-plane daemon.
///
/// Attaches to /dev/kestrel_ctl and handles IPC requests from the kernel module.
/// Metadata is selectable; block data is in-memory or persisted locally.
#[derive(Parser, Debug)]
#[command(name = "kestrelfs-daemon")]
#[command(version, about, long_about = None)]
struct Args {
    /// Directory for storing block data (objects) and metadata.
    ///
    /// If the directory doesn't exist, it will be created. Block data is stored
    /// as individual files organized by slice UUID. Metadata is stored in
    /// `meta.json` in the same directory. Defaults to "./.kestrelfs-data"
    /// in the current working directory.
    #[arg(long, default_value = "./.kestrelfs-data")]
    data_dir: PathBuf,

    /// Use in-memory storage for both metadata and objects (for testing).
    ///
    /// When enabled, all data is stored in RAM and lost on daemon restart.
    /// This is useful for tests but not recommended for production use.
    #[arg(long)]
    memory: bool,

    /// Redis URL for metadata, for example redis://127.0.0.1:6379/0.
    ///
    /// When omitted, metadata uses `{data_dir}/meta.json`. Redis metadata is
    /// incompatible with `--memory`; block objects remain in `data_dir`.
    #[arg(long, value_name = "REDIS_URL", conflicts_with = "memory")]
    meta: Option<String>,

    /// Namespace prefix used by the Redis metadata backend.
    #[arg(
        long,
        default_value = "kestrelfs",
        requires = "meta",
        conflicts_with = "memory"
    )]
    redis_prefix: String,

    /// PEM CA certificate used to verify a rediss:// metadata endpoint.
    ///
    /// Credentials still belong in `--meta`; neither value is logged.
    #[arg(
        long,
        value_name = "PEM_PATH",
        requires = "meta",
        conflicts_with = "memory"
    )]
    redis_ca_cert: Option<PathBuf>,

    /// Redis writer-session TTL in milliseconds. Mutations fail closed after
    /// expiry; heartbeat runs at one third of this interval.
    #[arg(
        long,
        default_value_t = 3000,
        value_parser = clap::value_parser!(u64).range(300..),
        requires = "meta",
        conflicts_with = "memory"
    )]
    redis_session_ttl_ms: u64,

    /// S3 object location, for example s3://bucket/optional/prefix.
    ///
    /// When omitted, persistent object data remains in `data_dir`.
    #[arg(long, value_name = "S3_URL", conflicts_with = "memory")]
    objects: Option<String>,

    /// Custom S3-compatible endpoint, for example http://127.0.0.1:9000.
    ///
    /// Primarily intended for MinIO. If omitted, `S3_ENDPOINT` is consulted,
    /// then the AWS SDK's normal S3 endpoint is used.
    #[arg(long, value_name = "URL", requires = "objects", conflicts_with = "memory")]
    s3_endpoint: Option<String>,
}

/// Seeds the single block backing `remote.txt`'s initial content.
///
/// The block key is derived from [`meta::REMOTE_TXT_SEED_SLICE_ID`]
/// (a nil UUID) and block index 0 (since the entire 512-byte file fits
/// in one block), following the [`Slice::block_key`] convention. The
/// content is a deterministic, human-readable pattern repeated to fill
/// exactly 512 bytes, chosen so `cat /mnt/kestrelfs/remote.txt` shows
/// something obviously different from the old Phase 2 synthetic string
/// ("KestrelFS remote chunk @ offset=...") that was generated on the fly.
///
/// This function is idempotent: if the block already exists in the ObjectStore
/// (e.g., from a previous daemon run with persistent storage), it is NOT
/// overwritten. This allows daemon restarts without losing seed data.
async fn seed_remote_txt_block(store: &Arc<dyn ObjectStore>) -> io::Result<()> {
    const BLOCK_SIZE: usize = 512;
    const PATTERN: &[u8] = b"Phase3-seed-data! ";

    let seed_slice = Slice {
        chunk_index: 0,
        slice_id: meta::REMOTE_TXT_SEED_SLICE_ID,
        chunk_offset: 0,
        length: BLOCK_SIZE as u32,
        written_at: 0,
    };
    let block_key = seed_slice.block_key(0);

    // Check if the block already exists and is complete. A malformed existing
    // seed is an integrity failure, not equivalent to an absent object.
    match store.get_exact(&block_key, BLOCK_SIZE).await {
        Ok(_) => return Ok(()),
        Err(object_store::ObjectStoreError::NotFound(_)) => {}
        Err(error) => return Err(io::Error::other(error.to_string())),
    }

    // Generate seed data
    let mut data = Vec::with_capacity(BLOCK_SIZE);
    while data.len() < BLOCK_SIZE {
        let remaining = BLOCK_SIZE - data.len();
        let chunk = &PATTERN[..remaining.min(PATTERN.len())];
        data.extend_from_slice(chunk);
    }

    store
        .put(block_key, data)
        .await
        .map_err(|e| io::Error::other(e.to_string()))?;

    Ok(())
}

fn main() -> io::Result<()> {
    abi::compile_time_layout_asserts();

    let args = Args::parse();

    println!("kestrelfs-daemon: opening /dev/kestrel_ctl ...");
    let dev = KestrelDevice::open()?;
    println!(
        "kestrelfs-daemon: attached OK (abi_version={}, region_size={} bytes)",
        abi::ABI_VERSION,
        abi::SHM_REGION_SIZE
    );

    // A Redis daemon may have been offline while another node committed
    // mutations. Retire every restored local entry before connecting to the
    // shared MetaStore, so an old cache cannot become a hit during startup.
    if args.meta.is_some() {
        dev.invalidate_cache_all()?;
        println!("kestrelfs-daemon: Redis coherence startup cache invalidation complete");
    }

    // A single, small multi-thread-capable Tokio runtime, created
    // once and kept alive for the process lifetime purely to
    // `block_on()` MetaStore calls from the synchronous event loop
    // below - see this module's doc comment for why the event loop
    // itself is not restructured to be async. `rt-multi-thread` is
    // enabled in Cargo.toml even though this bootstrap step's
    // Local MetaStores do not spawn additional work onto it, but the Redis
    // implementation uses Tokio-aware network I/O through this same runtime.
    let runtime = tokio::runtime::Runtime::new()?;

    // Initialize ObjectStore and MetaStore independently so File/Redis metadata
    // can be paired with LocalFs/S3 objects.
    let (object_store, store): (Arc<dyn ObjectStore>, Arc<dyn MetaStore>) = if args.memory {
        println!("kestrelfs-daemon: using in-memory storage (data will not persist)");
        (
            Arc::new(object_store::MemObjectStore::new()),
            Arc::new(meta::MemStore::new()),
        )
    } else {
        println!(
            "kestrelfs-daemon: using local filesystem storage at {}",
            args.data_dir.display()
        );
        let obj_store: Arc<dyn ObjectStore> = if let Some(location) = args.objects.as_deref() {
            let endpoint = args
                .s3_endpoint
                .clone()
                .or_else(|| std::env::var("S3_ENDPOINT").ok());
            println!("kestrelfs-daemon: using S3-compatible object storage");
            Arc::new(
                runtime
                    .block_on(object_store_s3::S3ObjectStore::new(
                        location,
                        endpoint.as_deref(),
                    ))
                    .map_err(|e| io::Error::other(e.to_string()))?,
            )
        } else {
            Arc::new(
                runtime
                    .block_on(object_store::LocalFsObjectStore::new(&args.data_dir))
                    .map_err(|e| io::Error::other(e.to_string()))?,
            )
        };
        let meta_store: Arc<dyn MetaStore> = if let Some(redis_url) = args.meta.as_deref() {
            // Do not print the URL: it may contain Redis credentials.
            println!(
                "kestrelfs-daemon: using Redis metadata (prefix={})",
                args.redis_prefix
            );
            let redis_ca = args
                .redis_ca_cert
                .as_ref()
                .map(std::fs::read)
                .transpose()?;
            let session_ttl = Duration::from_millis(args.redis_session_ttl_ms);
            let redis_store = match (redis_ca, session_ttl == meta_redis::DEFAULT_SESSION_TTL) {
                (None, true) => runtime.block_on(meta_redis::RedisMetaStore::new(
                    redis_url,
                    &args.redis_prefix,
                )),
                (Some(redis_ca), true) => {
                    runtime.block_on(meta_redis::RedisMetaStore::new_with_tls_ca(
                        redis_url,
                        &args.redis_prefix,
                        Some(redis_ca),
                    ))
                }
                (redis_ca, false) => {
                    runtime.block_on(meta_redis::RedisMetaStore::new_with_options(
                        redis_url,
                        &args.redis_prefix,
                        redis_ca,
                        session_ttl,
                    ))
                }
            };
            Arc::new(redis_store.map_err(|e| io::Error::other(e.to_string()))?)
        } else {
            let meta_path = args.data_dir.join("meta.json");
            Arc::new(
                runtime
                    .block_on(meta_persist::FileMetaStore::new(meta_path))
                    .map_err(|e| io::Error::other(e.to_string()))?,
            )
        };
        (obj_store, meta_store)
    };

    // Seed the block data for remote.txt's single Slice (see
    // meta::REMOTE_TXT_SEED_SLICE_ID). MemStore already created the
    // Slice metadata (inode 3, chunk 0, offset 0, length 512); we now
    // populate the corresponding block in the ObjectStore so reads can
    // actually return data instead of NotFound.
    runtime.block_on(seed_remote_txt_block(&object_store))?;

    // Ensure writable.dat (inode 4) exists. MemStore::new() pre-seeds it,
    // but we verify it here as a sanity check and to exercise the create()
    // trait method for clippy (avoiding dead_code warnings on MetaStore::create
    // and related MetaError variants).
    runtime.block_on(async {
        if store.getattr(meta::WRITABLE_DAT_INODE).await.is_err() {
            store
                .create(
                    fs_model::ROOT_INODE,
                    "writable.dat",
                    fs_model::S_IFREG | 0o666,
                )
                .await
                .expect("failed to create writable.dat");
            println!("kestrelfs-daemon: created /writable.dat (inode 4)");
        }
    });

    println!("kestrelfs-daemon: MetaStore initialized (seeded: /, /remote.txt, /writable.dat)");

    // Potentially slow ObjectStore deletes run away from the serial IPC ring
    // consumer. Only this thread acknowledges successful deletes in MetaStore,
    // so FileMetaStore remains a single-writer despite the background I/O.
    let mut gc_worker = GcWorker::start(runtime.handle(), Arc::clone(&object_store));
    let gc_scheduler = gc_worker.scheduler();
    GC_SCHEDULER
        .set(gc_scheduler.clone())
        .map_err(|_| io::Error::other("GC scheduler initialized more than once"))?;
    runtime.block_on(run_orphan_sweep(
        "startup",
        &dev,
        &store,
        &gc_scheduler,
    ));
    runtime.block_on(schedule_pending_garbage(
        "startup",
        &store,
        &gc_scheduler,
    ));

    println!("kestrelfs-daemon: entering poll() event loop, waiting for REQ events ...");

    event_loop(
        &dev,
        &runtime,
        &store,
        &object_store,
        &mut gc_worker,
    )
}

/// Blocks in `poll()` on the device fd until the kernel wakes us up
/// (via `wake_up_interruptible()` in `kestrelfs_req_push()`), then
/// drains and answers every pending REQ event, forever.
///
/// This is deliberately a simple, single-threaded loop for this
/// bootstrap step - see the module doc comment on `ring.rs` for why
/// that is sufficient for the current single-producer/single-consumer
/// role split.
fn event_loop(
    dev: &KestrelDevice,
    runtime: &tokio::runtime::Runtime,
    store: &Arc<dyn MetaStore>,
    object_store: &Arc<dyn ObjectStore>,
    gc_worker: &mut GcWorker,
) -> io::Result<()> {
    let notification_fd = store.coherence_notification_fd();
    let mut pfds = [
        libc::pollfd {
            fd: dev.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: notification_fd.unwrap_or(-1),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    let poll_count = if notification_fd.is_some() { 2 } else { 1 };

    let mut retry_delay = GC_RETRY_BASE;
    let mut retry_at = Instant::now() + retry_delay;
    let mut orphan_sweep_at = Instant::now() + ORPHAN_SWEEP_INTERVAL;
    let initial_coherence = runtime
        .block_on(store.coherence_probe(None))
        .map_err(|error| io::Error::other(format!("initial coherence probe failed: {error}")))?;
    let mut coherence_revision = initial_coherence.revision();
    let mut coherence_at = coherence_revision.map(|revision| {
        println!("kestrelfs-daemon: Redis coherence polling enabled at revision {revision}");
        Instant::now() + COHERENCE_POLL_INTERVAL
    });

    loop {
        for pfd in &mut pfds[..poll_count] {
            pfd.revents = 0;
        }

        let now = Instant::now();
        let wake_at = coherence_at
            .map_or(retry_at, |deadline| retry_at.min(deadline))
            .min(orphan_sweep_at);
        let timeout = wake_at
            .saturating_duration_since(now)
            .as_millis()
            .min(i32::MAX as u128) as i32;

        // SAFETY: `pfds` contains `poll_count` initialized pollfd values.
        // The bounded
        // timeout lets the same thread service durable GC retries while
        // idle. `poll()` itself performs no memory access beyond reading/writing
        // through this array pointer, which is safe C-ABI FFI as long
        // as the pointer and count agree, which they do here.
        let ret = unsafe { libc::poll(pfds.as_mut_ptr(), poll_count as libc::nfds_t, timeout) };

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

        if pfds[0].revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
            return Err(io::Error::other(format!(
                "kestrelfs-daemon: /dev/kestrel_ctl fd reported POLLERR/POLLNVAL (revents=0x{:x})",
                pfds[0].revents
            )));
        }
        if poll_count == 2 && pfds[1].revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
            return Err(io::Error::other(format!(
                "kestrelfs-daemon: Redis notification fd reported POLLERR/POLLNVAL (revents=0x{:x})",
                pfds[1].revents
            )));
        }

        if pfds[0].revents & libc::POLLIN != 0 {
            drain_and_respond(dev, runtime, store, object_store);
            // New mutations attempt GC inline. If that attempt failed, retry
            // promptly rather than inheriting an older long backoff. Taking
            // the earlier deadline (instead of assigning now + base) avoids
            // starving retry under a continuous request stream.
            retry_delay = GC_RETRY_BASE;
            retry_at = retry_at.min(Instant::now() + retry_delay);
        }

        if poll_count == 2
            && pfds[1].revents & libc::POLLIN != 0
            && store.drain_coherence_notifications()?
        {
            println!(
                "kestrelfs-daemon: Redis coherence notification received; reconciling durable revision"
            );
            reconcile_coherence(dev, runtime, store, &mut coherence_revision)?;
            coherence_at = coherence_revision.map(|_| Instant::now() + COHERENCE_POLL_INTERVAL);
        }

        while let Some(completion) = gc_worker.try_recv() {
            let retry_needed = runtime.block_on(apply_gc_completion(
                completion,
                store,
                &gc_worker.scheduler(),
            ));
            if retry_needed {
                retry_delay = next_gc_retry_delay(retry_delay, true);
            } else {
                retry_delay = GC_RETRY_BASE;
            }
            retry_at = Instant::now() + retry_delay;
        }

        if Instant::now() >= retry_at {
            let retry_needed = runtime.block_on(schedule_pending_garbage(
                "retry",
                store,
                &gc_worker.scheduler(),
            ));
            if retry_needed {
                retry_delay = next_gc_retry_delay(retry_delay, true);
            }
            retry_at = Instant::now() + retry_delay;
        }

        if coherence_at.is_some_and(|deadline| Instant::now() >= deadline) {
            reconcile_coherence(dev, runtime, store, &mut coherence_revision)?;
            coherence_at = coherence_revision.map(|_| Instant::now() + COHERENCE_POLL_INTERVAL);
        }

        if Instant::now() >= orphan_sweep_at {
            runtime.block_on(run_orphan_sweep(
                "periodic",
                dev,
                store,
                &gc_worker.scheduler(),
            ));
            orphan_sweep_at = Instant::now() + ORPHAN_SWEEP_INTERVAL;
        }
    }
}

fn reconcile_coherence(
    dev: &KestrelDevice,
    runtime: &tokio::runtime::Runtime,
    store: &Arc<dyn MetaStore>,
    coherence_revision: &mut Option<u64>,
) -> io::Result<()> {
    match runtime.block_on(store.coherence_probe(*coherence_revision)) {
        Ok(probe) => {
            let previous = *coherence_revision;
            match coherence_invalidation(Some(&probe)) {
                CoherenceInvalidation::None => {}
                CoherenceInvalidation::Inodes(inode_ids) => {
                    dev.invalidate_cache_inodes(&inode_ids)?;
                    println!(
                        "kestrelfs-daemon: coherence revision {} -> {}; invalidated {} dirty inode caches",
                        previous.unwrap_or_default(),
                        probe.revision().expect("inode probe has revision"),
                        inode_ids.len()
                    );
                }
                CoherenceInvalidation::All => {
                    dev.invalidate_cache_all()?;
                    println!(
                        "kestrelfs-daemon: coherence revision {} -> {}; dirty history unavailable/overflowed, full local cache invalidated",
                        previous.unwrap_or_default(),
                        probe.revision().expect("full probe has revision")
                    );
                }
            }
            *coherence_revision = probe.revision();
        }
        Err(error) => {
            // Metadata state is unknown, so retaining hits would be unsafe. A
            // full invalidation is fail-closed; keep the old revision and let
            // notification reconnect or the bounded poll retry the probe.
            eprintln!(
                "kestrelfs-daemon: coherence probe failed ({error}); invalidating local cache"
            );
            dev.invalidate_cache_all()?;
        }
    }
    Ok(())
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
fn drain_and_respond(
    dev: &KestrelDevice,
    runtime: &tokio::runtime::Runtime,
    store: &Arc<dyn MetaStore>,
    object_store: &Arc<dyn ObjectStore>,
) {
    // SAFETY: `dev.region_ptr()` returns the pointer established by
    // `KestrelDevice::open()`'s successful `mmap()`, which remains
    // valid for as long as `dev` (borrowed here) is alive - guaranteed
    // by this function's `&KestrelDevice` parameter outliving the
    // call. This is the daemon's single consumer thread for the REQ
    // ring (see `ring.rs` module doc comment), satisfying
    // `drain_requests`'s "no concurrent caller" safety requirement.
    // `runtime.block_on()` inside the closure below runs entirely on
    // this same thread (see this module's doc comment on why a Tokio
    // runtime is used here at all) - it never hands ring access to a
    // different thread, preserving that same single-consumer
    // invariant for the duration of this whole drain.
    let mut responded = 0u64;
    let drained = unsafe {
        ring::drain_requests(dev.region_ptr(), |event: &KestrelfsEvent| {
            println!(
                "kestrelfs-daemon: <- REQ  seq={} req_id={} opcode={} flags={}",
                event.seq, event.req_id, event.opcode, event.flags
            );

            // SAFETY: the ABI places the bounce buffer inside this live
            // mapping. DATA requests are synchronous and kernel-serialized,
            // so only the currently drained request may access it.
            let data_buffer = std::ptr::addr_of_mut!((*dev.region_ptr()).data_buffer).cast::<u8>();
            let response = runtime.block_on(build_response_with_data(
                event,
                store,
                object_store,
                data_buffer,
            ));

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
                    "kestrelfs-daemon: -> RESP req_id={} opcode={}",
                    event.req_id, response.opcode
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

/// Dispatches opcodes that access the shared bounce buffer, delegating all
/// legacy/control opcodes to `build_response`.
async fn build_response_with_data(
    event: &KestrelfsEvent,
    store: &Arc<dyn MetaStore>,
    object_store: &Arc<dyn ObjectStore>,
    data_buffer: *mut u8,
) -> KestrelfsEvent {
    match event.opcode {
        abi::OP_WRITE_DATA => handle_write_data(event, store, object_store, data_buffer).await,
        abi::OP_READ_DATA => handle_read_data(event, store, object_store, data_buffer).await,
        abi::OP_RENAME_DATA => {
            handle_rename_data(event, store, object_store, data_buffer).await
        }
        abi::OP_LOOKUP_DATA => handle_lookup_data(event, store, data_buffer).await,
        abi::OP_CREATE_DATA => handle_create_data(event, store, data_buffer).await,
        abi::OP_MKDIR_DATA => handle_mkdir_data(event, store, data_buffer).await,
        abi::OP_UNLINK_DATA => {
            handle_unlink_data(event, store, object_store, data_buffer).await
        }
        abi::OP_READDIR_DATA => handle_readdir_data(event, store, data_buffer).await,
        abi::OP_SYMLINK_DATA => handle_symlink_data(event, store, data_buffer).await,
        abi::OP_READLINK_DATA => handle_readlink_data(event, store, data_buffer).await,
        abi::OP_LINK_DATA => handle_link_data(event, store, data_buffer).await,
        _ => build_response(event, store, object_store).await,
    }
}

/// Builds the appropriate response event for one drained REQ event,
/// dispatching on `event.opcode`.
///
/// - `OP_LOOKUP`/`OP_GETATTR` are answered by querying `store` (see
///   [`handle_lookup`]/[`handle_getattr`]) - this is this daemon's
///   first real metadata-backed request handling (Phase 3 step 2).
/// - `OP_READ_CHUNK` still answers with the same deterministic,
///   synthetic payload it always has (see [`handle_read_chunk`]) -
///   wiring it to `store.read_slices()`/real block storage is
///   explicitly out of scope for this step.
/// - `OP_NOP` still answers with a bare `OP_RESULT_OK` (no payload
///   needed).
async fn build_response(
    event: &KestrelfsEvent,
    store: &Arc<dyn MetaStore>,
    object_store: &Arc<dyn ObjectStore>,
) -> KestrelfsEvent {
    match event.opcode {
        abi::OP_LOOKUP => handle_lookup(event, store).await,
        abi::OP_GETATTR => handle_getattr(event, store).await,
        abi::OP_READ_CHUNK => handle_read_chunk(event, store, object_store).await,
        abi::OP_WRITE_CHUNK => handle_write_chunk(event, store, object_store).await,
        abi::OP_TRUNCATE => handle_truncate(event, store, object_store).await,
        abi::OP_CREATE => handle_create(event, store).await,
        abi::OP_READDIR => handle_readdir(event, store).await,
        abi::OP_MKDIR => handle_mkdir(event, store).await,
        abi::OP_UNLINK => handle_unlink(event, store, object_store).await,
        abi::OP_RENAME => handle_rename(event, store, object_store).await,
        abi::OP_FINALIZE_ORPHAN => handle_finalize_orphan(event, store, object_store).await,
        abi::OP_SETATTR => handle_setattr(event, store).await,
        abi::OP_GETATTR_TIMES => handle_getattr_times(event, store).await,
        abi::OP_FSYNC | abi::OP_SYNC_FS => handle_sync(event, store, object_store).await,
        abi::OP_NOP => KestrelfsEvent::zeroed(abi::OP_RESULT_OK, event.req_id),
        other => {
            eprintln!(
                "kestrelfs-daemon: unhandled opcode {other} for req_id={}, replying with generic OK",
                event.req_id
            );
            KestrelfsEvent::zeroed(abi::OP_RESULT_OK, event.req_id)
        }
    }
}

/// Serialized IPC barrier: objects first, then authoritative metadata.
/// A missing/unreadable referenced local object fails closed with EIO.
async fn handle_sync(
    event: &KestrelfsEvent,
    store: &Arc<dyn MetaStore>,
    object_store: &Arc<dyn ObjectStore>,
) -> KestrelfsEvent {
    if event.flags != 0
        || (event.opcode == abi::OP_FSYNC && event.payload[8..].iter().any(|byte| *byte != 0))
        || (event.opcode == abi::OP_SYNC_FS && event.payload.iter().any(|byte| *byte != 0))
    {
        return KestrelfsEvent::error_response(event.req_id, -libc::EINVAL);
    }
    let keys = if event.opcode == abi::OP_FSYNC {
        let inode = u64::from_le_bytes(event.payload[..8].try_into().unwrap());
        store.referenced_keys(inode).await
    } else {
        store.all_referenced_keys().await
    };
    let keys = match keys {
        Ok(keys) => keys,
        Err(error) => return KestrelfsEvent::error_response(event.req_id, meta_error_to_errno(&error)),
    };
    let result = object_store.sync_keys(&keys).await;
    let result = if result.is_ok() && event.opcode == abi::OP_SYNC_FS {
        object_store.sync_all().await
    } else {
        result
    };
    if let Err(error) = result {
        eprintln!("kestrelfs-daemon: object sync failed: {error}");
        return KestrelfsEvent::error_response(event.req_id, -libc::EIO);
    }
    match store.sync_persistence().await {
        Ok(()) => KestrelfsEvent::zeroed(abi::OP_RESULT_OK, event.req_id),
        Err(error) => {
            eprintln!("kestrelfs-daemon: metadata sync failed: {error}");
            KestrelfsEvent::error_response(event.req_id, meta_error_to_errno(&error))
        }
    }
}

async fn handle_setattr(
    event: &KestrelfsEvent,
    store: &Arc<dyn MetaStore>,
) -> KestrelfsEvent {
    let req = event.decode_setattr_req();
    let has_basic = req.valid & abi::SETATTR_BASIC_MASK != 0;
    let has_times = req.valid & abi::SETATTR_TIME_MASK != 0;
    let reserved_nonzero = if has_times {
        event.payload[28..].iter().any(|byte| *byte != 0)
    } else {
        event.payload[24..].iter().any(|byte| *byte != 0)
    };
    if event.flags != 0
        || req.valid == 0
        || req.valid & !abi::SETATTR_VALID_MASK != 0
        || (has_basic && has_times)
        || reserved_nonzero
    {
        return KestrelfsEvent::error_response(event.req_id, -libc::EINVAL);
    }

    match store
        .set_attrs(
            req.inode_id,
            (req.valid & abi::SETATTR_MODE != 0).then_some(req.mode),
            (req.valid & abi::SETATTR_UID != 0).then_some(req.uid),
            (req.valid & abi::SETATTR_GID != 0).then_some(req.gid),
            (req.valid & abi::SETATTR_ATIME != 0).then_some(req.atime),
            (req.valid & abi::SETATTR_MTIME != 0).then_some(req.mtime),
        )
        .await
    {
        Ok(attrs) => {
            let mut response = KestrelfsEvent::zeroed(abi::OP_RESULT_OK, event.req_id);
            if has_times {
                response.payload[0..8].copy_from_slice(&attrs.atime.to_le_bytes());
                response.payload[8..16].copy_from_slice(&attrs.mtime.to_le_bytes());
            } else {
                response.payload[0..4].copy_from_slice(&attrs.mode.to_le_bytes());
                response.payload[4..8].copy_from_slice(&attrs.uid.to_le_bytes());
                response.payload[8..12].copy_from_slice(&attrs.gid.to_le_bytes());
            }
            response
        }
        Err(error) => {
            eprintln!(
                "kestrelfs-daemon:    OP_SETATTR inode={} valid={:#x} -> {error:?}",
                req.inode_id, req.valid
            );
            KestrelfsEvent::error_response(event.req_id, meta_error_to_errno(&error))
        }
    }
}

async fn handle_getattr_times(
    event: &KestrelfsEvent,
    store: &Arc<dyn MetaStore>,
) -> KestrelfsEvent {
    if event.flags != 0 || event.payload[8..].iter().any(|byte| *byte != 0) {
        return KestrelfsEvent::error_response(event.req_id, -libc::EINVAL);
    }
    let inode_id = u64::from_le_bytes(event.payload[0..8].try_into().unwrap());
    match store.getattr(inode_id).await {
        Ok(attrs) => {
            let mut response = KestrelfsEvent::zeroed(abi::OP_RESULT_OK, event.req_id);
            response.payload[0..8].copy_from_slice(&attrs.atime.to_le_bytes());
            response.payload[8..16].copy_from_slice(&attrs.mtime.to_le_bytes());
            response
        }
        Err(error) => {
            KestrelfsEvent::error_response(event.req_id, meta_error_to_errno(&error))
        }
    }
}

async fn handle_finalize_orphan(
    event: &KestrelfsEvent,
    store: &Arc<dyn MetaStore>,
    object_store: &Arc<dyn ObjectStore>,
) -> KestrelfsEvent {
    if event.flags != 0 || event.payload[8..].iter().any(|byte| *byte != 0) {
        return KestrelfsEvent::error_response(event.req_id, -libc::EINVAL);
    }
    let inode = u64::from_le_bytes(event.payload[0..8].try_into().unwrap());
    match store.finalize_orphan(inode).await {
        Ok(garbage_keys) => {
            dispatch_garbage_objects(
                "OP_FINALIZE_ORPHAN",
                &garbage_keys,
                store,
                object_store,
            )
            .await;
            KestrelfsEvent::zeroed(abi::OP_RESULT_OK, event.req_id)
        }
        Err(error) => {
            eprintln!("kestrelfs-daemon:    OP_FINALIZE_ORPHAN inode={inode} -> {error:?}");
            KestrelfsEvent::error_response(event.req_id, meta_error_to_errno(&error))
        }
    }
}

fn decode_name_data(
    event: &KestrelfsEvent,
    data_buffer: *const u8,
    operation: &str,
) -> Result<abi::NameDataReq, KestrelfsEvent> {
    // SAFETY: every caller is reached only from build_response_with_data,
    // which receives the live mmap pointer while the kernel owns the shared
    // data IPC mutex for the whole synchronous request.
    match unsafe { event.decode_name_data_req(data_buffer) } {
        Ok(req) => Ok(req),
        Err(abi::NameDataDecodeError::NameTooLong(_)) => {
            eprintln!(
                "kestrelfs-daemon:    {operation} req_id={} name too long",
                event.req_id
            );
            Err(KestrelfsEvent::error_response(
                event.req_id,
                -libc::ENAMETOOLONG,
            ))
        }
        Err(error) => {
            eprintln!(
                "kestrelfs-daemon:    {operation} req_id={} malformed: {error}",
                event.req_id
            );
            Err(KestrelfsEvent::error_response(
                event.req_id,
                -libc::EINVAL,
            ))
        }
    }
}

/// Handles `KESTRELFS_OP_CREATE` requests: creates a regular file or the
/// restricted metadata-only whiteout marker.
///
/// Decodes the request payload (parent_inode, mode, name), calls
/// [`MetaStore::create`], and returns the new inode's id + attributes.
///
/// Error mapping:
/// - Malformed payload (invalid UTF-8) -> `-EINVAL`
/// - [`MetaError::NotFound`] (parent doesn't exist) -> `-ENOENT`
/// - [`MetaError::NotADirectory`] (parent is not a directory) -> `-ENOTDIR`
/// - [`MetaError::AlreadyExists`] (name already exists in parent) -> `-EEXIST`
async fn handle_create(event: &KestrelfsEvent, store: &Arc<dyn MetaStore>) -> KestrelfsEvent {
    let req = match event.decode_create_req() {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "kestrelfs-daemon:    OP_CREATE malformed request: {:?}",
                e
            );
            return KestrelfsEvent::error_response(event.req_id, -libc::EINVAL);
        }
    };

    handle_create_request(event.req_id, "OP_CREATE", req, store).await
}

async fn handle_create_data(
    event: &KestrelfsEvent,
    store: &Arc<dyn MetaStore>,
    data_buffer: *const u8,
) -> KestrelfsEvent {
    let req = match decode_name_data(event, data_buffer, "OP_CREATE_DATA") {
        Ok(req) => abi::CreateReq {
            parent_inode: req.parent_inode,
            mode: req.mode,
            name: req.name,
        },
        Err(response) => return response,
    };
    handle_create_request(event.req_id, "OP_CREATE_DATA", req, store).await
}

async fn handle_create_request(
    req_id: u64,
    operation: &str,
    req: abi::CreateReq,
    store: &Arc<dyn MetaStore>,
) -> KestrelfsEvent {
    println!(
        "kestrelfs-daemon:    {operation} parent={} name=\"{}\" mode=0o{:o}",
        req.parent_inode, req.name, req.mode
    );

    // Call MetaStore::create
    match store.create(req.parent_inode, &req.name, req.mode).await {
        Ok(new_inode_id) => {
            // Fetch attributes of newly created inode
            match store.getattr(new_inode_id).await {
                Ok(inode) => {
                    let attrs = inode_to_attr_fields(&inode);
                    println!(
                        "kestrelfs-daemon:    OP_CREATE -> new_inode={} size={} mode=0o{:o}",
                        new_inode_id, attrs.size, attrs.mode
                    );
                    // Response layout same as LOOKUP (child_inode_id + attrs)
                    KestrelfsEvent::lookup_response(req_id, new_inode_id, attrs)
                }
                Err(e) => {
                    eprintln!(
                        "kestrelfs-daemon:    OP_CREATE created inode={} but getattr failed: {:?}",
                        new_inode_id, e
                    );
                    KestrelfsEvent::error_response(req_id, -libc::EIO)
                }
            }
        }
        Err(e) => {
            println!(
                "kestrelfs-daemon:    OP_CREATE parent={} name=\"{}\" -> {:?}",
                req.parent_inode, req.name, e
            );
            KestrelfsEvent::error_response(req_id, meta_error_to_errno(&e))
        }
    }
}

async fn handle_symlink_data(
    event: &KestrelfsEvent,
    store: &Arc<dyn MetaStore>,
    data_buffer: *mut u8,
) -> KestrelfsEvent {
    // SAFETY: this handler is called with the live mmap bounce buffer while
    // the kernel holds the data IPC mutex for the synchronous transaction.
    let req = match unsafe { event.decode_symlink_data_req(data_buffer.cast_const()) } {
        Ok(req) => req,
        Err(
            abi::SymlinkDataDecodeError::NameTooLong(_)
            | abi::SymlinkDataDecodeError::TargetTooLong(_)
            | abi::SymlinkDataDecodeError::CombinedDataTooLong,
        ) => return KestrelfsEvent::error_response(event.req_id, -libc::ENAMETOOLONG),
        Err(error) => {
            eprintln!(
                "kestrelfs-daemon:    OP_SYMLINK_DATA req_id={} malformed: {error}",
                event.req_id
            );
            return KestrelfsEvent::error_response(event.req_id, -libc::EINVAL);
        }
    };

    let inode_id = match store
        .symlink(req.parent_inode, &req.name, &req.target)
        .await
    {
        Ok(inode_id) => inode_id,
        Err(error) => {
            return KestrelfsEvent::error_response(event.req_id, meta_error_to_errno(&error));
        }
    };
    match store.getattr(inode_id).await {
        Ok(inode) => KestrelfsEvent::lookup_response(
            event.req_id,
            inode_id,
            inode_to_attr_fields(&inode),
        ),
        Err(_) => KestrelfsEvent::error_response(event.req_id, -libc::EIO),
    }
}

async fn handle_readlink_data(
    event: &KestrelfsEvent,
    store: &Arc<dyn MetaStore>,
    data_buffer: *mut u8,
) -> KestrelfsEvent {
    if event.flags != 0 {
        return KestrelfsEvent::error_response(event.req_id, -libc::EINVAL);
    }
    let inode_id = u64::from_le_bytes(event.payload[0..8].try_into().unwrap());
    let target = match store.readlink(inode_id).await {
        Ok(target) => target,
        Err(error) => {
            return KestrelfsEvent::error_response(event.req_id, meta_error_to_errno(&error));
        }
    };
    if target.len() > abi::SYMLINK_TARGET_MAX || target.len() > abi::DATA_BUFFER_SIZE {
        return KestrelfsEvent::error_response(event.req_id, -libc::EIO);
    }

    // SAFETY: target is bounded by DATA_BUFFER_SIZE and the mmap remains live
    // for the complete synchronous request.
    unsafe { std::ptr::copy_nonoverlapping(target.as_ptr(), data_buffer, target.len()) };
    let mut response = KestrelfsEvent::zeroed(abi::OP_RESULT_OK, event.req_id);
    response.payload[0..4].copy_from_slice(&(target.len() as u32).to_le_bytes());
    response
}

async fn handle_link_data(
    event: &KestrelfsEvent,
    store: &Arc<dyn MetaStore>,
    data_buffer: *const u8,
) -> KestrelfsEvent {
    // SAFETY: the kernel keeps kestrelfs_data_ipc_lock held until this
    // synchronous request has received its response.
    let req = match unsafe { event.decode_link_data_req(data_buffer) } {
        Ok(req) => req,
        Err(abi::NameDataDecodeError::NameTooLong(_)) => {
            return KestrelfsEvent::error_response(event.req_id, -libc::ENAMETOOLONG);
        }
        Err(error) => {
            eprintln!(
                "kestrelfs-daemon:    OP_LINK_DATA req_id={} malformed: {error}",
                event.req_id
            );
            return KestrelfsEvent::error_response(event.req_id, -libc::EINVAL);
        }
    };

    match store.link(req.parent_inode, &req.name, req.inode_id).await {
        Ok(nlink) => {
            let mut response = KestrelfsEvent::zeroed(abi::OP_RESULT_OK, event.req_id);
            response.payload[0..4].copy_from_slice(&nlink.to_le_bytes());
            response
        }
        Err(error) => {
            KestrelfsEvent::error_response(event.req_id, meta_error_to_errno(&error))
        }
    }
}

/// Handles `KESTRELFS_OP_READDIR` requests: lists directory entries.
///
/// Decodes the request payload (dir_inode, offset), calls
/// [`MetaStore::readdir`], and returns up to 2 entries per response
/// (limited by payload size).
///
/// Error mapping:
/// - [`MetaError::NotFound`] -> `-ENOENT`
/// - [`MetaError::NotADirectory`] -> `-ENOTDIR`
async fn handle_readdir(event: &KestrelfsEvent, store: &Arc<dyn MetaStore>) -> KestrelfsEvent {
    let req = event.decode_readdir_req();

    println!(
        "kestrelfs-daemon:    OP_READDIR dir={} offset={}",
        req.dir_inode, req.offset
    );

    // Call MetaStore::readdir
    match store.readdir(req.dir_inode).await {
        Ok(entries) => {
            // Sort entries by inode for stable ordering (HashMap is non-deterministic)
            let mut sorted_entries: Vec<(u64, &str)> = entries
                .iter()
                .map(|entry| (entry.inode_id, entry.name.as_str()))
                .collect();
            sorted_entries.sort_by_key(|(inode, _)| *inode);
            
            // Skip to requested offset and take only 1 entry (to avoid name truncation)
            let offset = req.offset as usize;
            let chunk: Vec<(u64, &str)> = sorted_entries
                .iter()
                .skip(offset)
                .take(1)
                .copied()
                .collect();

            println!(
                "kestrelfs-daemon:    OP_READDIR dir={} offset={} -> {} entries (total {} in dir)",
                req.dir_inode,
                req.offset,
                chunk.len(),
                entries.len()
            );

            KestrelfsEvent::readdir_response(event.req_id, &chunk)
        }
        Err(e) => {
            println!(
                "kestrelfs-daemon:    OP_READDIR dir={} offset={} -> {:?}",
                req.dir_inode, req.offset, e
            );
            KestrelfsEvent::error_response(event.req_id, meta_error_to_errno(&e))
        }
    }
}

async fn handle_readdir_data(
    event: &KestrelfsEvent,
    store: &Arc<dyn MetaStore>,
    data_buffer: *mut u8,
) -> KestrelfsEvent {
    if event.flags != 0 {
        return KestrelfsEvent::error_response(event.req_id, -libc::EINVAL);
    }

    let req = event.decode_readdir_req();
    let entries = match store.readdir(req.dir_inode).await {
        Ok(entries) => entries,
        Err(error) => {
            return KestrelfsEvent::error_response(event.req_id, meta_error_to_errno(&error));
        }
    };

    let mut entries = entries;
    entries.sort_by_key(|entry| entry.inode_id);
    let mut encoded = Vec::with_capacity(abi::DATA_BUFFER_SIZE);
    let mut entry_count = 0u32;

    for entry in entries.iter().skip(req.offset as usize) {
        let name = entry.name.as_bytes();
        if name.is_empty() || name.len() > abi::NAME_DATA_MAX {
            return KestrelfsEvent::error_response(event.req_id, -libc::EIO);
        }
        let dtype = match entry.mode & fs_model::S_IFMT {
            fs_model::S_IFREG => libc::DT_REG,
            fs_model::S_IFDIR => libc::DT_DIR,
            fs_model::S_IFLNK => libc::DT_LNK,
            fs_model::S_IFCHR => libc::DT_CHR,
            _ => return KestrelfsEvent::error_response(event.req_id, -libc::EIO),
        };
        let record_len = abi::READDIR_DATA_ENTRY_HEADER_SIZE + name.len();
        if encoded.len() + record_len > abi::DATA_BUFFER_SIZE {
            break;
        }
        encoded.extend_from_slice(&entry.inode_id.to_le_bytes());
        encoded.extend_from_slice(&(name.len() as u16).to_le_bytes());
        encoded.push(dtype);
        encoded.push(0);
        encoded.extend_from_slice(name);
        entry_count += 1;
    }

    // A valid 255-byte maximum entry always fits in the 16 KiB buffer, so a
    // non-EOF request must make progress.
    if entry_count == 0 && (req.offset as usize) < entries.len() {
        return KestrelfsEvent::error_response(event.req_id, -libc::EIO);
    }

    // SAFETY: build_response_with_data supplies a live DATA_BUFFER_SIZE-byte
    // mapping and the kernel retains exclusive data-IPC ownership. encoded is
    // bounded above by that exact size.
    unsafe {
        std::ptr::copy_nonoverlapping(encoded.as_ptr(), data_buffer, encoded.len());
    }

    let mut response = KestrelfsEvent::zeroed(abi::OP_RESULT_OK, event.req_id);
    response.payload[0..4].copy_from_slice(&entry_count.to_le_bytes());
    response.payload[4..8].copy_from_slice(&(encoded.len() as u32).to_le_bytes());
    response
}

async fn handle_mkdir(event: &KestrelfsEvent, store: &Arc<dyn MetaStore>) -> KestrelfsEvent {
    let mut parent_inode_bytes = [0u8; 8];
    let mut mode_bytes = [0u8; 4];
    let mut name_bytes = [0u8; 20];

    // Decode request: parent_inode(u64@0) + mode(u32@8) + name(NUL-terminated@12)
    parent_inode_bytes.copy_from_slice(&event.payload[0..8]);
    mode_bytes.copy_from_slice(&event.payload[8..12]);
    name_bytes.copy_from_slice(&event.payload[12..32]);

    let parent_inode = u64::from_le_bytes(parent_inode_bytes);
    let mode = u32::from_le_bytes(mode_bytes);

    // Extract NUL-terminated name
    let name = match std::ffi::CStr::from_bytes_until_nul(&name_bytes) {
        Ok(cstr) => match cstr.to_str() {
            Ok(s) => s,
            Err(_) => {
                eprintln!("kestrelfs-daemon:    OP_MKDIR invalid UTF-8 name");
                return KestrelfsEvent::error_response(event.req_id, -libc::EINVAL);
            }
        },
        Err(_) => {
            eprintln!("kestrelfs-daemon:    OP_MKDIR name not NUL-terminated");
            return KestrelfsEvent::error_response(event.req_id, -libc::EINVAL);
        }
    };

    handle_mkdir_request(event.req_id, "OP_MKDIR", parent_inode, mode, name, store).await
}

async fn handle_mkdir_data(
    event: &KestrelfsEvent,
    store: &Arc<dyn MetaStore>,
    data_buffer: *const u8,
) -> KestrelfsEvent {
    let req = match decode_name_data(event, data_buffer, "OP_MKDIR_DATA") {
        Ok(req) => req,
        Err(response) => return response,
    };
    handle_mkdir_request(
        event.req_id,
        "OP_MKDIR_DATA",
        req.parent_inode,
        req.mode,
        &req.name,
        store,
    )
    .await
}

async fn handle_mkdir_request(
    req_id: u64,
    operation: &str,
    parent_inode: u64,
    mode: u32,
    name: &str,
    store: &Arc<dyn MetaStore>,
) -> KestrelfsEvent {
    println!(
        "kestrelfs-daemon:    {operation} parent={parent_inode} name=\"{name}\" mode=0o{mode:o}"
    );

    // Call MetaStore::mkdir
    match store.mkdir(parent_inode, name, mode).await {
        Ok(new_inode_id) => {
            println!(
                "kestrelfs-daemon:    OP_MKDIR created dir inode={}",
                new_inode_id
            );
            // Response: new_inode_id(u64@0)
            let mut resp = KestrelfsEvent::zeroed(abi::OP_RESULT_OK, req_id);
            resp.payload[0..8].copy_from_slice(&new_inode_id.to_le_bytes());
            resp
        }
        Err(e) => {
            println!(
                "kestrelfs-daemon:    OP_MKDIR parent={} name=\"{}\" -> {:?}",
                parent_inode, name, e
            );
            KestrelfsEvent::error_response(req_id, meta_error_to_errno(&e))
        }
    }
}

async fn handle_unlink(
    event: &KestrelfsEvent,
    store: &Arc<dyn MetaStore>,
    object_store: &Arc<dyn ObjectStore>,
) -> KestrelfsEvent {
    let mut parent_inode_bytes = [0u8; 8];
    let mut name_bytes = [0u8; 24];

    // Decode request: parent_inode(u64@0) + name(NUL-terminated@8)
    parent_inode_bytes.copy_from_slice(&event.payload[0..8]);
    name_bytes.copy_from_slice(&event.payload[8..32]);

    let parent_inode = u64::from_le_bytes(parent_inode_bytes);

    // Extract NUL-terminated name
    let name = match std::ffi::CStr::from_bytes_until_nul(&name_bytes) {
        Ok(cstr) => match cstr.to_str() {
            Ok(s) => s,
            Err(_) => {
                eprintln!("kestrelfs-daemon:    OP_UNLINK invalid UTF-8 name");
                return KestrelfsEvent::error_response(event.req_id, -libc::EINVAL);
            }
        },
        Err(_) => {
            eprintln!("kestrelfs-daemon:    OP_UNLINK name not NUL-terminated");
            return KestrelfsEvent::error_response(event.req_id, -libc::EINVAL);
        }
    };

    handle_unlink_request(
        event.req_id,
        "OP_UNLINK",
        parent_inode,
        name,
        false,
        store,
        object_store,
    )
    .await
}

async fn handle_unlink_data(
    event: &KestrelfsEvent,
    store: &Arc<dyn MetaStore>,
    object_store: &Arc<dyn ObjectStore>,
    data_buffer: *const u8,
) -> KestrelfsEvent {
    let req = match decode_name_data(event, data_buffer, "OP_UNLINK_DATA") {
        Ok(req) => req,
        Err(response) => return response,
    };
    if req.mode & !abi::LIFECYCLE_DEFER_RECLAIM != 0 {
        return KestrelfsEvent::error_response(event.req_id, -libc::EINVAL);
    }
    handle_unlink_request(
        event.req_id,
        "OP_UNLINK_DATA",
        req.parent_inode,
        &req.name,
        req.mode & abi::LIFECYCLE_DEFER_RECLAIM != 0,
        store,
        object_store,
    )
    .await
}

async fn handle_unlink_request(
    req_id: u64,
    operation: &str,
    parent_inode: u64,
    name: &str,
    defer_reclaim: bool,
    store: &Arc<dyn MetaStore>,
    object_store: &Arc<dyn ObjectStore>,
) -> KestrelfsEvent {
    println!("kestrelfs-daemon:    {operation} parent={parent_inode} name=\"{name}\"");

    // Call MetaStore::unlink
    match store
        .unlink_with_lifecycle(parent_inode, name, defer_reclaim)
        .await
    {
        Ok(garbage_keys) => {
            dispatch_garbage_objects(operation, &garbage_keys, store, object_store).await;
            println!(
                "kestrelfs-daemon:    OP_UNLINK removed \"{}\" from parent={}",
                name, parent_inode
            );
            KestrelfsEvent::zeroed(abi::OP_RESULT_OK, req_id)
        }
        Err(e) => {
            println!(
                "kestrelfs-daemon:    OP_UNLINK parent={} name=\"{}\" -> {:?}",
                parent_inode, name, e
            );
            KestrelfsEvent::error_response(req_id, meta_error_to_errno(&e))
        }
    }
}

async fn handle_rename(
    event: &KestrelfsEvent,
    store: &Arc<dyn MetaStore>,
    object_store: &Arc<dyn ObjectStore>,
) -> KestrelfsEvent {
    let req = match event.decode_rename_req() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("kestrelfs-daemon:    OP_RENAME decode error: {:?}", e);
            return KestrelfsEvent::error_response(event.req_id, -libc::EINVAL);
        }
    };

    handle_rename_request(event.req_id, "OP_RENAME", req, store, object_store).await
}

async fn handle_rename_data(
    event: &KestrelfsEvent,
    store: &Arc<dyn MetaStore>,
    object_store: &Arc<dyn ObjectStore>,
    data_buffer: *const u8,
) -> KestrelfsEvent {
    // SAFETY: the event loop receives the pointer from the live shared mapping.
    // The kernel keeps kestrelfs_data_ipc_lock held until this response is
    // consumed, so this request has exclusive ownership of the bounce buffer.
    let req = match unsafe { event.decode_rename_data_req(data_buffer) } {
        Ok(req) => req,
        Err(
            abi::RenameDataDecodeError::OldNameTooLong(_)
            | abi::RenameDataDecodeError::NewNameTooLong(_)
            | abi::RenameDataDecodeError::CombinedNamesTooLong,
        ) => {
            eprintln!(
                "kestrelfs-daemon:    OP_RENAME_DATA req_id={} name too long",
                event.req_id
            );
            return KestrelfsEvent::error_response(event.req_id, -libc::ENAMETOOLONG);
        }
        Err(error) => {
            eprintln!(
                "kestrelfs-daemon:    OP_RENAME_DATA req_id={} malformed: {error}",
                event.req_id
            );
            return KestrelfsEvent::error_response(event.req_id, -libc::EINVAL);
        }
    };

    handle_rename_request(
        event.req_id,
        "OP_RENAME_DATA",
        req,
        store,
        object_store,
    )
    .await
}

async fn handle_rename_request(
    req_id: u64,
    operation: &str,
    req: abi::RenameReq,
    store: &Arc<dyn MetaStore>,
    object_store: &Arc<dyn ObjectStore>,
) -> KestrelfsEvent {
    println!(
        "kestrelfs-daemon:    {operation} old_parent={} old_name=\"{}\" new_parent={} new_name=\"{}\" flags={:#x}",
        req.old_parent, req.old_name, req.new_parent, req.new_name, req.flags
    );

    match store
        .rename_with_lifecycle(
            req.old_parent,
            &req.old_name,
            req.new_parent,
            &req.new_name,
            req.flags,
            req.defer_reclaim,
        )
        .await
    {
        Ok(garbage_keys) => {
            dispatch_garbage_objects(operation, &garbage_keys, store, object_store).await;
            println!(
                "kestrelfs-daemon:    {operation} success: \"{}\" -> \"{}\"",
                req.old_name, req.new_name
            );
            KestrelfsEvent::zeroed(abi::OP_RESULT_OK, req_id)
        }
        Err(e) => {
            println!("kestrelfs-daemon:    {operation} error: {e:?}");
            KestrelfsEvent::error_response(req_id, meta_error_to_errno(&e))
        }
    }
}

/// Sends committed garbage to the production worker without waiting for
/// ObjectStore latency. Unit tests do not run `main()`, so they retain the
/// direct helper's deterministic, immediate-delete behavior.
async fn dispatch_garbage_objects(
    operation: &str,
    garbage_keys: &[String],
    store: &Arc<dyn MetaStore>,
    object_store: &Arc<dyn ObjectStore>,
) {
    if let Some(scheduler) = GC_SCHEDULER.get() {
        let outcome = scheduler.schedule(operation, garbage_keys.to_vec());
        if outcome.backpressured != 0 {
            eprintln!(
                "kestrelfs-daemon:    DIST-OBJECT source={operation} worker queue full; {} key(s) remain durable for retry",
                outcome.backpressured
            );
        }
        return;
    }

    let outcome = delete_garbage_objects(operation, Some(garbage_keys), store, object_store).await;
    if outcome.retry_needed {
        eprintln!(
            "kestrelfs-daemon:    DIST-OBJECT source={operation} direct test path requires retry"
        );
    }
}

/// Reads the durable queue on the IPC thread and attempts a nonblocking worker
/// submission. Returning true asks the event loop to retain retry backoff.
async fn schedule_pending_garbage(
    operation: &str,
    store: &Arc<dyn MetaStore>,
    scheduler: &GcScheduler,
) -> bool {
    let pending = match store.pending_garbage().await {
        Ok(keys) => keys,
        Err(error) => {
            let failures = GC_FAILURES.fetch_add(1, Ordering::Relaxed) + 1;
            eprintln!(
                "kestrelfs-daemon:    DIST-OBJECT source={operation} queue read failed: {error}; total_failures={failures}"
            );
            return true;
        }
    };
    let outcome = scheduler.schedule(operation, pending);
    if outcome.backpressured != 0 {
        eprintln!(
            "kestrelfs-daemon:    DIST-OBJECT source={operation} worker queue full; {} key(s) remain durable for retry",
            outcome.backpressured
        );
        return true;
    }
    false
}

/// Replays only final-close failures recorded by the kernel after its local
/// open-handle count reached zero. Peek/ack makes the queue crash-safe across
/// daemon restarts while the module stays loaded; no session-expiry guess is
/// accepted as open-reference evidence.
async fn run_orphan_sweep(
    source: &str,
    dev: &KestrelDevice,
    store: &Arc<dyn MetaStore>,
    scheduler: &GcScheduler,
) {
    let mut reclaimed = 0_usize;
    for _ in 0..64 {
        let inode = match dev.peek_orphan_retry() {
            Ok(Some(inode)) => inode,
            Ok(None) => break,
            Err(error) => {
                eprintln!("kestrelfs-daemon: ORPHAN-SWEEP source={source} peek failed: {error}");
                break;
            }
        };
        let garbage = match store.finalize_orphan(inode).await {
            Ok(garbage) => garbage,
            Err(MetaError::NotFound) => Vec::new(),
            Err(error) => {
                eprintln!(
                    "kestrelfs-daemon: ORPHAN-SWEEP source={source} inode={inode} retained for retry: {error}"
                );
                break;
            }
        };
        if let Err(error) = dev.acknowledge_orphan_retry(inode) {
            eprintln!(
                "kestrelfs-daemon: ORPHAN-SWEEP source={source} inode={inode} ack failed: {error}"
            );
            break;
        }
        reclaimed += 1;
        let scheduled = scheduler.schedule("orphan-sweep", garbage);
        if scheduled.backpressured != 0 {
            eprintln!(
                "kestrelfs-daemon: ORPHAN-SWEEP worker queue full; {} key(s) remain durable",
                scheduled.backpressured
            );
        }
    }
    if reclaimed != 0 {
        println!(
            "kestrelfs-daemon: ORPHAN-SWEEP source={source} reclaimed={reclaimed} proof=kernel-final-close"
        );
    }
}

/// Applies one worker result on the serial IPC/metadata thread. Successful
/// deletes are acknowledged only here; failures (including ack failure) stay
/// in the durable queue and become schedulable again.
async fn apply_gc_completion(
    completion: DeleteCompletion,
    store: &Arc<dyn MetaStore>,
    scheduler: &GcScheduler,
) -> bool {
    let pass = GC_PASSES.fetch_add(1, Ordering::Relaxed) + 1;
    let attempted = completion.attempted();
    GC_ATTEMPTED.fetch_add(attempted as u64, Ordering::Relaxed);
    for (key, error) in &completion.failed {
        eprintln!(
            "kestrelfs-daemon:    DIST-OBJECT source={} delete failed for {key}: {error}",
            completion.operation
        );
    }

    let mut acknowledged = 0usize;
    let mut ack_failed = false;
    if !completion.succeeded.is_empty() {
        if let Err(error) = store.acknowledge_garbage(&completion.succeeded).await {
            ack_failed = true;
            eprintln!(
                "kestrelfs-daemon:    DIST-OBJECT source={} queue ack failed after {} idempotent delete(s): {error}",
                completion.operation,
                completion.succeeded.len()
            );
        } else {
            acknowledged = completion.succeeded.len();
            GC_DELETED.fetch_add(acknowledged as u64, Ordering::Relaxed);
        }
    }

    let failures = completion.failed.len() as u64 + u64::from(ack_failed);
    GC_FAILURES.fetch_add(failures, Ordering::Relaxed);
    scheduler.complete(&completion);
    println!(
        "kestrelfs-daemon:    DIST-OBJECT pass={pass} source={} attempted={attempted} acknowledged={acknowledged} failed={failures}; totals attempted={} deleted={} failures={}",
        completion.operation,
        GC_ATTEMPTED.load(Ordering::Relaxed),
        GC_DELETED.load(Ordering::Relaxed),
        GC_FAILURES.load(Ordering::Relaxed)
    );
    failures != 0
}

#[derive(Default)]
struct GcPassOutcome {
    retry_needed: bool,
}

/// Runs one at-least-once GC pass. Candidates were committed into MetaStore's
/// durable queue in the same transaction that removed their last reference.
/// Each pass revalidates the queue against all current slices, deletes objects
/// idempotently, then acknowledges only successful deletes. A crash between
/// delete and acknowledgement causes a harmless repeated delete after restart.
async fn delete_garbage_objects(
    operation: &str,
    requested_keys: Option<&[String]>,
    store: &Arc<dyn MetaStore>,
    object_store: &Arc<dyn ObjectStore>,
) -> GcPassOutcome {
    let pass = GC_PASSES.fetch_add(1, Ordering::Relaxed) + 1;
    let pending = match store.pending_garbage().await {
        Ok(keys) => keys,
        Err(error) => {
            let failures = GC_FAILURES.fetch_add(1, Ordering::Relaxed) + 1;
            eprintln!(
                "kestrelfs-daemon:    DIST-GC pass={pass} source={operation} queue read failed: {error}; total_failures={failures}"
            );
            return GcPassOutcome { retry_needed: true };
        }
    };
    let requested: Option<HashSet<&str>> = requested_keys
        .map(|keys| keys.iter().map(String::as_str).collect());
    let eligible: Vec<_> = pending
        .into_iter()
        .filter(|key| {
            requested
                .as_ref()
                .is_none_or(|keys| keys.contains(key.as_str()))
        })
        .collect();
    if eligible.is_empty() {
        return GcPassOutcome::default();
    }

    let attempted = eligible.len();
    let mut acknowledged = Vec::new();
    let mut delete_failures = 0u64;
    for key in eligible {
        GC_ATTEMPTED.fetch_add(1, Ordering::Relaxed);
        match object_store.delete(&key).await {
            Ok(()) => acknowledged.push(key),
            Err(error) => {
                delete_failures += 1;
                eprintln!(
                    "kestrelfs-daemon:    DIST-GC source={operation} delete failed for {key}: {error}"
                );
            }
        }
    }

    let mut ack_failed = false;
    let mut acknowledged_count = 0usize;
    if !acknowledged.is_empty() {
        if let Err(error) = store.acknowledge_garbage(&acknowledged).await {
            ack_failed = true;
            eprintln!(
                "kestrelfs-daemon:    DIST-GC source={operation} queue ack failed after {} idempotent delete(s): {error}",
                acknowledged.len()
            );
        } else {
            acknowledged_count = acknowledged.len();
            GC_DELETED.fetch_add(acknowledged.len() as u64, Ordering::Relaxed);
        }
    }
    let failures = delete_failures + u64::from(ack_failed);
    GC_FAILURES.fetch_add(failures, Ordering::Relaxed);
    println!(
        "kestrelfs-daemon:    DIST-GC pass={pass} source={operation} attempted={attempted} acknowledged={acknowledged_count} failed={failures}; totals attempted={} deleted={} failures={}",
        GC_ATTEMPTED.load(Ordering::Relaxed),
        GC_DELETED.load(Ordering::Relaxed),
        GC_FAILURES.load(Ordering::Relaxed)
    );
    GcPassOutcome {
        retry_needed: failures != 0,
    }
}

/// Converts a [`MetaError`] into the negative errno-style value the
/// event header's `error_code` field carries on a
/// `KESTRELFS_OP_RESULT_ERROR` response (see `kestrelfs_ipc.h`'s doc
/// comment on why errors never use the payload).
///
/// Uses `libc`'s errno constants (already a dependency for the
/// `ioctl`/`poll`/`mmap` FFI calls elsewhere in this daemon) rather
/// than hand-rolled magic numbers, so this mapping stays correct
/// automatically across any target `libc` is itself built for.
fn meta_error_to_errno(err: &MetaError) -> i32 {
    match err {
        MetaError::NotFound => -libc::ENOENT,
        MetaError::NotADirectory => -libc::ENOTDIR,
        MetaError::InvalidName(_) => -libc::ENAMETOOLONG,
        MetaError::AlreadyExists => -libc::EEXIST,
        MetaError::NotEmpty => -libc::ENOTEMPTY,
        MetaError::NotASymlink => -libc::EINVAL,
        MetaError::IsADirectory => -libc::EPERM,
        MetaError::TooManyLinks => -libc::EMLINK,
        MetaError::UnsupportedRenameFlags(_) => -libc::EINVAL,
        MetaError::UnsupportedNodeType(_) => -libc::EOPNOTSUPP,
        MetaError::Io => -libc::EIO,
        MetaError::StaleSession => -libc::ESTALE,
    }
}

/// Converts a fully-resolved [`Inode`] into the wire-format
/// [`AttrFields`] every `OP_LOOKUP`/`OP_GETATTR` success response is
/// built from.
///
/// A small, explicit conversion function - deliberately not a `From`/
/// `Into` impl living in `abi.rs` itself, to keep that module fully
/// decoupled from `fs_model` (see `AttrFields`'s doc comment in
/// `abi.rs` for the reasoning); this function is the one place that
/// bridges the two.
fn inode_to_attr_fields(inode: &Inode) -> AttrFields {
    AttrFields {
        size: inode.size,
        mode: inode.mode,
        uid: inode.uid,
        gid: inode.gid,
        nlink: inode.nlink,
        mtime: inode.mtime,
    }
}

/// Answers a `KESTRELFS_OP_LOOKUP` request: decodes `(parent_inode,
/// name)` from the request payload (see
/// [`KestrelfsEvent::decode_lookup_req`]), resolves the child's inode
/// id via [`MetaStore::lookup`], then immediately fetches that
/// child's full attributes via [`MetaStore::getattr`] so the response
/// carries complete attributes in one round trip - mirroring how a
/// real `lookup(2)`-backing VFS operation returns a full `struct
/// stat`, not just an inode number (see `kestrelfs_ipc.h`'s doc
/// comment on `MetaStore` method selection).
///
/// # Error mapping
///
/// - A malformed request payload (name too long for the wire format,
///   or not valid UTF-8 - see [`abi::LookupDecodeError`]) is mapped to
///   `-ENAMETOOLONG`/`-EINVAL` respectively, without ever calling into
///   `store` at all.
/// - [`MetaError::NotFound`] (no such parent, or no child with this
///   name) -> `-ENOENT`.
/// - [`MetaError::NotADirectory`] (parent exists but is a regular
///   file) -> `-ENOTDIR`.
async fn handle_lookup(event: &KestrelfsEvent, store: &Arc<dyn MetaStore>) -> KestrelfsEvent {
    let req = match event.decode_lookup_req() {
        Ok(req) => req,
        Err(abi::LookupDecodeError::NameTooLong(len)) => {
            eprintln!(
                "kestrelfs-daemon:    OP_LOOKUP req_id={} malformed: name_len={len} exceeds LOOKUP_NAME_MAX",
                event.req_id
            );
            return KestrelfsEvent::error_response(event.req_id, -libc::ENAMETOOLONG);
        }
        Err(abi::LookupDecodeError::InvalidUtf8) => {
            eprintln!(
                "kestrelfs-daemon:    OP_LOOKUP req_id={} malformed: name is not valid UTF-8",
                event.req_id
            );
            return KestrelfsEvent::error_response(event.req_id, -libc::EINVAL);
        }
    };

    handle_lookup_request(event.req_id, "OP_LOOKUP", req.parent_inode, &req.name, store).await
}

async fn handle_lookup_data(
    event: &KestrelfsEvent,
    store: &Arc<dyn MetaStore>,
    data_buffer: *const u8,
) -> KestrelfsEvent {
    let req = match decode_name_data(event, data_buffer, "OP_LOOKUP_DATA") {
        Ok(req) => req,
        Err(response) => return response,
    };
    handle_lookup_request(
        event.req_id,
        "OP_LOOKUP_DATA",
        req.parent_inode,
        &req.name,
        store,
    )
    .await
}

async fn handle_lookup_request(
    req_id: u64,
    operation: &str,
    parent_inode: u64,
    name: &str,
    store: &Arc<dyn MetaStore>,
) -> KestrelfsEvent {
    println!("kestrelfs-daemon:    {operation} parent={parent_inode} name={name:?}");

    let child_inode_id = match store.lookup(parent_inode, name).await {
        Ok(id) => id,
        Err(err) => {
            let errno = meta_error_to_errno(&err);
            eprintln!(
                "kestrelfs-daemon:    {operation} parent={parent_inode} name={name:?} -> error: {err} (errno {errno})"
            );
            return KestrelfsEvent::error_response(req_id, errno);
        }
    };

    let inode = match store.getattr(child_inode_id).await {
        Ok(inode) => inode,
        Err(err) => {
            // The child id we just resolved a moment ago no longer
            // has attributes - only possible under concurrent
            // mutation of the store (not yet possible with the
            // current read-only MetaStore trait, but handled
            // defensively rather than assumed impossible).
            let errno = meta_error_to_errno(&err);
            eprintln!(
                "kestrelfs-daemon:    OP_LOOKUP resolved child_inode_id={child_inode_id} but getattr failed: {err} (errno {errno})"
            );
            return KestrelfsEvent::error_response(req_id, errno);
        }
    };

    println!(
        "kestrelfs-daemon:    {operation} parent={parent_inode} name={name:?} -> inode={child_inode_id} size={} mode={:o}",
        inode.size, inode.mode
    );

    KestrelfsEvent::lookup_response(req_id, child_inode_id, inode_to_attr_fields(&inode))
}

/// Answers a `KESTRELFS_OP_GETATTR` request: decodes `inode_id` from
/// the request payload (see [`KestrelfsEvent::decode_getattr_req`])
/// and resolves its attributes via [`MetaStore::getattr`].
///
/// # Error mapping
///
/// [`MetaError::NotFound`] -> `-ENOENT`. `NotADirectory`/`InvalidName`
/// are not reachable from `getattr()` per the `MetaStore` trait's own
/// contract (only `lookup()` can produce them), but
/// [`meta_error_to_errno`] maps them anyway for completeness/future-proofing
/// rather than this function special-casing which variants it
/// "expects".
async fn handle_getattr(event: &KestrelfsEvent, store: &Arc<dyn MetaStore>) -> KestrelfsEvent {
    let req = event.decode_getattr_req();

    println!("kestrelfs-daemon:    OP_GETATTR inode={}", req.inode_id);

    let inode = match store.getattr(req.inode_id).await {
        Ok(inode) => inode,
        Err(err) => {
            let errno = meta_error_to_errno(&err);
            eprintln!(
                "kestrelfs-daemon:    OP_GETATTR inode={} -> error: {err} (errno {errno})",
                req.inode_id
            );
            return KestrelfsEvent::error_response(event.req_id, errno);
        }
    };

    println!(
        "kestrelfs-daemon:    OP_GETATTR inode={} -> size={} mode={:o} nlink={}",
        req.inode_id, inode.size, inode.mode, inode.nlink
    );

    KestrelfsEvent::getattr_response(event.req_id, inode_to_attr_fields(&inode))
}

/// Answers a `KESTRELFS_OP_READ_CHUNK` request by fetching the
/// requested byte range from the backing store (MetaStore for slice
/// metadata + ObjectStore for block data), introduced in Phase 3 step 3.
///
/// This replaces the Phase 2 stub that synthesized "KestrelFS remote
/// chunk @ offset=..." strings on the fly. As of this step:
/// - `req.inode_id` (added in KESTRELFS_ABI_VERSION 2) identifies which
///   file to read.
/// - `req.offset`/`req.count` specify the byte range (already clamped
///   by the kernel to fit in one response payload - see
///   `kestrelfs_remote_read()` in kestrelfs/file.c).
/// - We look up the file's slices in MetaStore, find which slice(s)
///   cover the requested range, fetch the corresponding block(s) from
///   ObjectStore, and copy the relevant bytes into the response payload.
/// - Any byte within the file's declared `size` that is NOT covered by
///   a slice (a "hole") is returned as zero (sparse-file semantics).
/// - If `req.inode_id` does not exist, or `req.offset` is beyond the
///   file's size, return `-ENOENT`.
///
/// For this Phase 3 step, only `remote.txt` (inode 3) has any slices
/// seeded (one Slice covering 0..512), with the selected ObjectStore serving
/// its bytes. A future Phase 4 will generalize this to
/// arbitrary files, multi-block slices, and network-backed stores.
async fn handle_read_chunk(
    event: &KestrelfsEvent,
    store: &Arc<dyn MetaStore>,
    object_store: &Arc<dyn ObjectStore>,
) -> KestrelfsEvent {
    let req = event.decode_read_chunk_req();

    // Step 1: getattr to validate the inode exists and get its size
    let inode = match store.getattr(req.inode_id).await {
        Ok(inode) => inode,
        Err(MetaError::NotFound) => {
            println!(
                "kestrelfs-daemon:    OP_READ_CHUNK inode={} offset={} count={} -> ENOENT (inode not found)",
                req.inode_id, req.offset, req.count
            );
            return KestrelfsEvent::error_response(
                event.req_id,
                meta_error_to_errno(&MetaError::NotFound),
            );
        }
        Err(e) => {
            eprintln!(
                "kestrelfs-daemon:    OP_READ_CHUNK inode={} offset={} count={} -> EIO (getattr failed: {e})",
                req.inode_id, req.offset, req.count
            );
            return KestrelfsEvent::error_response(event.req_id, -libc::EIO);
        }
    };

    // Step 2: EOF check (offset beyond file size -> return 0 bytes,
    // which the kernel interprets as EOF)
    if req.offset >= inode.size {
        println!(
            "kestrelfs-daemon:    OP_READ_CHUNK inode={} offset={} count={} -> 0 bytes (EOF, size={})",
            req.inode_id, req.offset, req.count, inode.size
        );
        return KestrelfsEvent::read_chunk_response(event.req_id, &[]);
    }

    // Step 3: Clamp count to not read past EOF
    let clamped_count = ((inode.size - req.offset) as u32).min(req.count);

    // Step 4: read_from_slices helper does the heavy lifting (find
    // covering slices, fetch blocks, assemble bytes)
    match read_from_slices(req.inode_id, req.offset, clamped_count, store, object_store).await {
        Ok(data) => {
            println!(
                "kestrelfs-daemon:    OP_READ_CHUNK inode={} offset={} count={} -> {} bytes",
                req.inode_id,
                req.offset,
                req.count,
                data.len()
            );
            KestrelfsEvent::read_chunk_response(event.req_id, &data)
        }
        Err(e) => {
            eprintln!(
                "kestrelfs-daemon:    OP_READ_CHUNK inode={} offset={} count={} -> EIO ({e})",
                req.inode_id, req.offset, req.count
            );
            KestrelfsEvent::error_response(event.req_id, -libc::EIO)
        }
    }
}

/// Handles `OP_WRITE_CHUNK` requests: writes data to a file.
///
/// Steps:
/// 1. Decode the write request (inode_id, offset, count, data)
/// 2. Validate inode exists (getattr)
/// 3. Generate a new slice UUID and timestamp
/// 4. Store the data block in ObjectStore under the slice's block key
/// 5. Append the slice to MetaStore (updates size/mtime atomically)
/// 6. Return success response
async fn handle_write_chunk(
    event: &KestrelfsEvent,
    store: &Arc<dyn MetaStore>,
    object_store: &Arc<dyn ObjectStore>,
) -> KestrelfsEvent {
    let req = event.decode_write_chunk_req();
    debug_assert_eq!(req.count as usize, req.data.len());

    handle_write_bytes(
        event.req_id,
        req.inode_id,
        req.offset,
        req.data,
        "OP_WRITE_CHUNK",
        store,
        object_store,
    )
    .await
}

/// Handles ABI v8 `OP_WRITE_DATA`, copying the requested bytes out of the
/// shared bounce buffer before reusing the legacy write/slice implementation.
async fn handle_write_data(
    event: &KestrelfsEvent,
    store: &Arc<dyn MetaStore>,
    object_store: &Arc<dyn ObjectStore>,
    data_buffer: *mut u8,
) -> KestrelfsEvent {
    let req = event.decode_data_req();
    if req.length as usize > abi::DATA_BUFFER_SIZE {
        return KestrelfsEvent::error_response(event.req_id, -libc::EINVAL);
    }

    let mut data = vec![0u8; req.length as usize];
    // SAFETY: `data_buffer` points to the mapped ABI bounce buffer and the
    // validated length is in-bounds. The kernel retains ownership until the
    // request-ring release publication, which the daemon consumed before
    // reaching this handler.
    unsafe {
        std::ptr::copy_nonoverlapping(data_buffer.cast_const(), data.as_mut_ptr(), data.len());
    }

    handle_write_bytes(
        event.req_id,
        req.inode_id,
        req.offset,
        data,
        "OP_WRITE_DATA",
        store,
        object_store,
    )
    .await
}

/// Stores one write as a new COW slice. Both the legacy inline opcode and the
/// ABI v8 bounce-buffer opcode use this path, preserving MetaStore/ObjectStore
/// semantics.
async fn handle_write_bytes(
    req_id: u64,
    inode_id: u64,
    offset: u64,
    data: Vec<u8>,
    operation: &str,
    store: &Arc<dyn MetaStore>,
    object_store: &Arc<dyn ObjectStore>,
) -> KestrelfsEvent {
    let count = data.len() as u32;

    // Step 1: Validate inode exists
    if let Err(e) = store.getattr(inode_id).await {
        println!(
            "kestrelfs-daemon:    {operation} inode={inode_id} offset={offset} count={count} -> {e:?}"
        );
        return KestrelfsEvent::error_response(req_id, meta_error_to_errno(&e));
    }

    // Step 2: Calculate chunk index and chunk offset
    let chunk_index = (offset / fs_model::CHUNK_SIZE) as u32;
    let chunk_offset = (offset % fs_model::CHUNK_SIZE) as u32;

    // Step 3: Create a new slice
    let slice_id = uuid::Uuid::new_v4();
    let written_at = meta::current_unix_time();
    let slice = fs_model::Slice {
        chunk_index,
        slice_id,
        chunk_offset,
        length: count,
        written_at,
    };

    // Step 4: Store the data block in ObjectStore
    let block_key = slice.block_key(0);
    if let Err(e) = object_store.put(block_key.clone(), data).await {
        eprintln!(
            "kestrelfs-daemon:    {operation} inode={inode_id} offset={offset} count={count} -> EIO (ObjectStore::put failed: {e:?})"
        );
        return KestrelfsEvent::error_response(req_id, -libc::EIO);
    }

    // Step 5: Append slice to MetaStore (updates size and mtime)
    if let Err(e) = store.append_slice(inode_id, slice).await {
        eprintln!(
            "kestrelfs-daemon:    {operation} inode={inode_id} offset={offset} count={count} -> EIO (MetaStore::append_slice failed: {e:?})"
        );
        return KestrelfsEvent::error_response(req_id, -libc::EIO);
    }

    println!(
        "kestrelfs-daemon:    {operation} inode={inode_id} offset={offset} count={count} -> success (slice_id={slice_id})"
    );

    KestrelfsEvent::zeroed(abi::OP_RESULT_OK, req_id)
}

/// Handles ABI v8 `OP_READ_DATA`. The existing slice assembly logic fills the
/// bounce buffer, while RESULT_OK payload[0..4] reports the actual byte count.
async fn handle_read_data(
    event: &KestrelfsEvent,
    store: &Arc<dyn MetaStore>,
    object_store: &Arc<dyn ObjectStore>,
    data_buffer: *mut u8,
) -> KestrelfsEvent {
    let req = event.decode_data_req();
    if req.length as usize > abi::DATA_BUFFER_SIZE {
        return KestrelfsEvent::error_response(event.req_id, -libc::EINVAL);
    }

    if let Err(e) = store.getattr(req.inode_id).await {
        return KestrelfsEvent::error_response(event.req_id, meta_error_to_errno(&e));
    }

    let data =
        match read_from_slices(req.inode_id, req.offset, req.length, store, object_store).await {
            Ok(data) => data,
            Err(e) => {
                eprintln!(
                    "kestrelfs-daemon:    OP_READ_DATA inode={} offset={} length={} -> EIO ({e})",
                    req.inode_id, req.offset, req.length
                );
                return KestrelfsEvent::error_response(event.req_id, -libc::EIO);
            }
        };

    // SAFETY: the request length was validated against the mapped buffer and
    // `read_from_slices` never returns more than that length. This write is
    // ordered before response-ring publication by its Release store.
    unsafe {
        std::ptr::copy_nonoverlapping(data.as_ptr(), data_buffer, data.len());
    }

    let actual = data.len() as u32;
    let mut response = KestrelfsEvent::zeroed(abi::OP_RESULT_OK, event.req_id);
    response.payload[0..4].copy_from_slice(&actual.to_le_bytes());
    println!(
        "kestrelfs-daemon:    OP_READ_DATA inode={} offset={} length={} -> {} bytes",
        req.inode_id, req.offset, req.length, actual
    );
    response
}

/// Handles `OP_TRUNCATE` requests: sets file size (truncate/ftruncate).
///
/// Steps:
/// 1. Decode the truncate request (inode_id, new_size)
/// 2. Validate inode exists (getattr)
/// 3. Call MetaStore::truncate to update size and mtime
/// 4. Return success response
///
/// The MetaStore implementation updates size/mtime, drops slices wholly past
/// retained EOF, and shortens crossing slices. Confirmed-unreferenced block
/// keys are then deleted from ObjectStore after the metadata commit.
async fn handle_truncate(
    event: &KestrelfsEvent,
    store: &Arc<dyn MetaStore>,
    object_store: &Arc<dyn ObjectStore>,
) -> KestrelfsEvent {
    let req = event.decode_truncate_req();

    // Step 1: Validate inode exists
    if let Err(e) = store.getattr(req.inode_id).await {
        println!(
            "kestrelfs-daemon:    OP_TRUNCATE inode={} new_size={} -> {:?}",
            req.inode_id, req.new_size, e
        );
        return KestrelfsEvent::error_response(event.req_id, meta_error_to_errno(&e));
    }

    // Step 2: Truncate the file
    let garbage_keys = match store.truncate(req.inode_id, req.new_size).await {
        Ok(keys) => keys,
        Err(e) => {
            eprintln!(
                "kestrelfs-daemon:    OP_TRUNCATE inode={} new_size={} -> EIO (MetaStore::truncate failed: {:?})",
                req.inode_id, req.new_size, e
            );
            return KestrelfsEvent::error_response(event.req_id, -libc::EIO);
        }
    };
    dispatch_garbage_objects("OP_TRUNCATE", &garbage_keys, store, object_store).await;

    println!(
        "kestrelfs-daemon:    OP_TRUNCATE inode={} new_size={} -> success",
        req.inode_id, req.new_size
    );

    KestrelfsEvent::zeroed(abi::OP_RESULT_OK, event.req_id)
}

/// Reads `count` bytes starting at file offset `file_offset` from inode
/// `inode_id`, consulting the MetaStore's slice metadata and fetching
/// block data from the ObjectStore.
///
/// Returns a `Vec<u8>` of exactly `count` bytes. Any byte not covered
/// by a slice (a "hole" in the file) is returned as 0. If multiple
/// slices overlap the same byte, the slice with the greatest
/// `written_at` timestamp wins (copy-on-write / last-write-wins
/// semantics, per the fs_model.rs module doc).
///
/// # Errors
///
/// Returns `Err` if the MetaStore's `read_slices` call fails, or if any
/// required block is missing from the ObjectStore. The caller should
/// surface these as `-EIO` to the kernel.
async fn read_from_slices(
    inode_id: u64,
    file_offset: u64,
    count: u32,
    store: &Arc<dyn MetaStore>,
    object_store: &Arc<dyn ObjectStore>,
) -> Result<Vec<u8>, String> {
    use std::collections::HashMap;

    // Get inode to check file size (respects truncate)
    let inode = store
        .getattr(inode_id)
        .await
        .map_err(|e| format!("getattr(inode={inode_id}) failed: {e}"))?;

    // Clamp read to file size (POSIX semantics: reads beyond EOF return short)
    if file_offset >= inode.size {
        return Ok(Vec::new()); // EOF
    }

    let available = inode.size - file_offset;
    let clamped_count = (count as u64).min(available) as u32;

    let mut result = vec![0u8; clamped_count as usize];

    // Block cache: avoid fetching the same block multiple times in one read
    let mut block_cache: HashMap<String, Vec<u8>> = HashMap::new();

    // Identify which chunk(s) this read spans (Phase 3 bootstrap: we
    // only ever read within one chunk since kernel clamps to 32 bytes,
    // but the logic here is written generically for future expansion).
    let start_chunk = Inode::chunk_index_for_offset(file_offset);
    let end_offset = file_offset + clamped_count as u64;
    let end_chunk = if end_offset == 0 {
        start_chunk
    } else {
        Inode::chunk_index_for_offset(end_offset - 1)
    };

    for chunk_idx in start_chunk..=end_chunk {
        let slices = store
            .read_slices(inode_id, chunk_idx)
            .await
            .map_err(|e| format!("read_slices(inode={inode_id}, chunk={chunk_idx}) failed: {e}"))?;

        if slices.is_empty() {
            continue;
        }

        // For each byte in this chunk that our read range touches, find
        // the covering slice with the greatest written_at (if any)
        let chunk_base = chunk_idx as u64 * fs_model::CHUNK_SIZE;
        let read_start_in_chunk = file_offset.saturating_sub(chunk_base);
        let read_end_in_chunk = (end_offset.saturating_sub(chunk_base)).min(fs_model::CHUNK_SIZE);

        for offset_in_chunk in read_start_in_chunk..read_end_in_chunk {
            let covering_slice = slices
                .iter()
                .filter(|s| {
                    let s_start = s.chunk_offset as u64;
                    let s_end = s_start + s.length as u64;
                    offset_in_chunk >= s_start && offset_in_chunk < s_end
                })
                .max_by_key(|s| s.written_at);

            if let Some(slice) = covering_slice {
                let byte_in_slice = offset_in_chunk - slice.chunk_offset as u64;
                let block_idx = (byte_in_slice / fs_model::BLOCK_SIZE) as u32;
                let byte_in_block = (byte_in_slice % fs_model::BLOCK_SIZE) as usize;

                let block_key = slice.block_key(block_idx);

                // Check cache first, fetch if not present
                if !block_cache.contains_key(&block_key) {
                    let block_start = u64::from(block_idx) * fs_model::BLOCK_SIZE;
                    let expected_len = (u64::from(slice.length) - block_start)
                        .min(fs_model::BLOCK_SIZE) as usize;
                    let block_data = object_store
                        .get_range(&block_key, expected_len, fs_model::BLOCK_SIZE as usize)
                        .await
                        .map_err(|e| format!("ObjectStore::get_range({block_key}) failed: {e}"))?;
                    block_cache.insert(block_key.clone(), block_data);
                }

                let block_data = &block_cache[&block_key];
                let file_byte_idx = (chunk_base + offset_in_chunk - file_offset) as usize;
                result[file_byte_idx] = block_data[byte_in_block];
            }
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta_persist::FileMetaStore;
    use crate::object_store::{LocalFsObjectStore, ObjectStoreError};
    use crate::object_store_s3::S3ObjectStore;
    use meta::{MemStore, REMOTE_TXT_INODE};
    use std::sync::atomic::AtomicUsize;
    use tempfile::TempDir;

    #[derive(Clone)]
    struct FailOnceObjectStore {
        inner: object_store::MemObjectStore,
        failures_left: Arc<AtomicUsize>,
    }

    impl FailOnceObjectStore {
        fn new() -> Self {
            Self {
                inner: object_store::MemObjectStore::new(),
                failures_left: Arc::new(AtomicUsize::new(1)),
            }
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for FailOnceObjectStore {
        async fn get(&self, key: &str) -> object_store::Result<Vec<u8>> {
            self.inner.get(key).await
        }

        async fn put(&self, key: String, value: Vec<u8>) -> object_store::Result<()> {
            self.inner.put(key, value).await
        }

        async fn delete(&self, key: &str) -> object_store::Result<()> {
            if self.failures_left.swap(0, Ordering::SeqCst) != 0 {
                return Err(ObjectStoreError::Io("injected delete failure".to_string()));
            }
            self.inner.delete(key).await
        }
    }

    #[test]
    fn gc_retry_delay_backs_off_and_caps() {
        assert_eq!(
            next_gc_retry_delay(GC_RETRY_BASE, true),
            Duration::from_secs(2)
        );
        assert_eq!(
            next_gc_retry_delay(Duration::from_secs(32), true),
            GC_RETRY_MAX
        );
        assert_eq!(next_gc_retry_delay(GC_RETRY_MAX, true), GC_RETRY_MAX);
        assert_eq!(
            next_gc_retry_delay(Duration::from_secs(16), false),
            GC_RETRY_BASE
        );
    }

    #[test]
    fn coherence_probe_selects_fine_full_and_failure_fallbacks() {
        assert_eq!(
            coherence_invalidation(Some(&CoherenceProbe::Unchanged { revision: 7 })),
            CoherenceInvalidation::None
        );
        assert_eq!(
            coherence_invalidation(Some(&CoherenceProbe::Inodes {
                revision: 8,
                inode_ids: vec![3, 9],
            })),
            CoherenceInvalidation::Inodes(vec![3, 9])
        );
        assert_eq!(
            coherence_invalidation(Some(&CoherenceProbe::Full { revision: 9 })),
            CoherenceInvalidation::All
        );
        assert_eq!(coherence_invalidation(None), CoherenceInvalidation::All);
    }

    #[test]
    fn cli_selects_metadata_and_object_backends() {
        let default_args = Args::try_parse_from(["kestrelfs-daemon"]).unwrap();
        assert!(!default_args.memory);
        assert!(default_args.meta.is_none());
        assert!(default_args.objects.is_none());
        assert_eq!(default_args.redis_session_ttl_ms, 3000);

        let memory_args = Args::try_parse_from(["kestrelfs-daemon", "--memory"]).unwrap();
        assert!(memory_args.memory);
        assert!(memory_args.meta.is_none());

        let distributed_args = Args::try_parse_from([
            "kestrelfs-daemon",
            "--meta",
            "redis://127.0.0.1:6379/0",
            "--redis-prefix",
            "test-fs",
            "--objects",
            "s3://test-bucket/test-prefix",
            "--s3-endpoint",
            "http://127.0.0.1:9000",
        ])
        .unwrap();
        assert_eq!(
            distributed_args.meta.as_deref(),
            Some("redis://127.0.0.1:6379/0")
        );
        assert_eq!(distributed_args.redis_prefix, "test-fs");
        assert_eq!(distributed_args.redis_session_ttl_ms, 3000);
        assert!(distributed_args.redis_ca_cert.is_none());
        assert_eq!(
            distributed_args.objects.as_deref(),
            Some("s3://test-bucket/test-prefix")
        );
        assert_eq!(
            distributed_args.s3_endpoint.as_deref(),
            Some("http://127.0.0.1:9000")
        );

        assert!(Args::try_parse_from([
            "kestrelfs-daemon",
            "--memory",
            "--meta",
            "redis://127.0.0.1:6379/0",
        ])
        .is_err());
        assert!(Args::try_parse_from([
            "kestrelfs-daemon",
            "--memory",
            "--objects",
            "s3://test-bucket/prefix",
        ])
        .is_err());
        assert!(Args::try_parse_from([
            "kestrelfs-daemon",
            "--s3-endpoint",
            "http://127.0.0.1:9000",
        ])
        .is_err());
        assert!(Args::try_parse_from([
            "kestrelfs-daemon",
            "--redis-ca-cert",
            "/tmp/test-ca.pem",
        ])
        .is_err());
        assert!(Args::try_parse_from([
            "kestrelfs-daemon",
            "--meta",
            "redis://127.0.0.1:6379/0",
            "--redis-session-ttl-ms",
            "299",
        ])
        .is_err());

        let tls_args = Args::try_parse_from([
            "kestrelfs-daemon",
            "--meta",
            "rediss://redis.example.test:6379/0",
            "--redis-ca-cert",
            "/etc/kestrelfs/redis-ca.pem",
        ])
        .unwrap();
        assert_eq!(
            tls_args.redis_ca_cert.as_deref(),
            Some(std::path::Path::new("/etc/kestrelfs/redis-ca.pem"))
        );
    }

    /// Builds a raw `OP_LOOKUP` request event, encoding `(parent_inode,
    /// name)` by hand at the exact wire offsets documented in
    /// `kestrelfs_ipc.h` - deliberately independent of any
    /// encoder/decoder in `abi.rs`, so these `build_response` tests
    /// validate against the wire format itself, not against whatever
    /// `abi.rs` happens to (correctly or incorrectly) produce.
    fn raw_lookup_req(req_id: u64, parent_inode: u64, name: &str) -> KestrelfsEvent {
        let mut event = KestrelfsEvent::zeroed(abi::OP_LOOKUP, req_id);
        event.payload[0..8].copy_from_slice(&parent_inode.to_le_bytes());
        let name_bytes = name.as_bytes();
        event.payload[8] = name_bytes.len() as u8;
        event.payload[9..9 + name_bytes.len()].copy_from_slice(name_bytes);
        event
    }

    /// Builds a raw ABI v8 READ_DATA/WRITE_DATA request.
    fn raw_data_req(
        opcode: u32,
        req_id: u64,
        inode_id: u64,
        offset: u64,
        length: u32,
    ) -> KestrelfsEvent {
        let mut event = KestrelfsEvent::zeroed(opcode, req_id);
        event.payload[0..8].copy_from_slice(&inode_id.to_le_bytes());
        event.payload[8..16].copy_from_slice(&offset.to_le_bytes());
        event.payload[16..20].copy_from_slice(&length.to_le_bytes());
        event
    }

    #[tokio::test]
    async fn data_opcodes_round_trip_100_and_4096_bytes() {
        let store: Arc<dyn MetaStore> = Arc::new(MemStore::new());
        let object_store: Arc<dyn ObjectStore> =
            Arc::new(object_store::MemObjectStore::new());

        for (case, length) in [100usize, 4096].into_iter().enumerate() {
            let inode_id = store
                .create(
                    fs_model::ROOT_INODE,
                    &format!("bulk-{length}.bin"),
                    fs_model::S_IFREG | 0o644,
                )
                .await
                .expect("create must succeed");
            let expected: Vec<u8> = (0..length).map(|i| (i % 251) as u8).collect();
            let mut bounce = [0u8; abi::DATA_BUFFER_SIZE];
            bounce[..length].copy_from_slice(&expected);

            let write = raw_data_req(
                abi::OP_WRITE_DATA,
                10_000 + case as u64,
                inode_id,
                0,
                length as u32,
            );
            let response =
                handle_write_data(&write, &store, &object_store, bounce.as_mut_ptr()).await;
            assert_eq!(response.opcode, abi::OP_RESULT_OK);

            bounce[..length].fill(0);
            let read = raw_data_req(
                abi::OP_READ_DATA,
                20_000 + case as u64,
                inode_id,
                0,
                length as u32,
            );
            let response =
                handle_read_data(&read, &store, &object_store, bounce.as_mut_ptr()).await;
            assert_eq!(response.opcode, abi::OP_RESULT_OK);
            assert_eq!(
                u32::from_le_bytes(response.payload[0..4].try_into().unwrap()),
                length as u32
            );
            assert_eq!(&bounce[..length], expected.as_slice());
        }
    }

    #[tokio::test]
    async fn data_opcode_round_trip_survives_file_meta_store_reload() {
        let dir = TempDir::new().unwrap();
        let meta_path = dir.path().join("meta.json");
        let expected: Vec<u8> = (0..4096).map(|i| ((i * 7) % 253) as u8).collect();
        let inode_id;

        {
            let store: Arc<dyn MetaStore> =
                Arc::new(FileMetaStore::new(meta_path.clone()).await.unwrap());
            let object_store: Arc<dyn ObjectStore> =
                Arc::new(LocalFsObjectStore::new(dir.path()).await.unwrap());
            inode_id = store
                .create(
                    fs_model::ROOT_INODE,
                    "persistent-bulk.bin",
                    fs_model::S_IFREG | 0o644,
                )
                .await
                .unwrap();
            let mut bounce = [0u8; abi::DATA_BUFFER_SIZE];
            bounce[..expected.len()].copy_from_slice(&expected);
            let write = raw_data_req(
                abi::OP_WRITE_DATA,
                30_000,
                inode_id,
                0,
                expected.len() as u32,
            );
            let response =
                handle_write_data(&write, &store, &object_store, bounce.as_mut_ptr()).await;
            assert_eq!(response.opcode, abi::OP_RESULT_OK);
        }

        let store: Arc<dyn MetaStore> =
            Arc::new(FileMetaStore::new(meta_path).await.unwrap());
        let object_store: Arc<dyn ObjectStore> =
            Arc::new(LocalFsObjectStore::new(dir.path()).await.unwrap());
        assert_eq!(
            store
                .lookup(fs_model::ROOT_INODE, "persistent-bulk.bin")
                .await
                .unwrap(),
            inode_id
        );

        let mut bounce = [0u8; abi::DATA_BUFFER_SIZE];
        let read = raw_data_req(
            abi::OP_READ_DATA,
            30_001,
            inode_id,
            0,
            expected.len() as u32,
        );
        let response = handle_read_data(&read, &store, &object_store, bounce.as_mut_ptr()).await;
        assert_eq!(response.opcode, abi::OP_RESULT_OK);
        assert_eq!(&bounce[..expected.len()], expected.as_slice());
    }

    #[tokio::test]
    async fn fsync_and_sync_fs_barrier_survive_local_store_reopen() {
        let dir = TempDir::new().unwrap();
        let meta_path = dir.path().join("meta.json");
        let store: Arc<dyn MetaStore> = Arc::new(FileMetaStore::new(meta_path.clone()).await.unwrap());
        let objects: Arc<dyn ObjectStore> =
            Arc::new(LocalFsObjectStore::new(dir.path()).await.unwrap());
        seed_remote_txt_block(&objects).await.unwrap();
        let inode = store.create(fs_model::ROOT_INODE, "sync-data", 0o644).await.unwrap();
        let content = b"step44 durable object and metadata".to_vec();
        assert_eq!(
            handle_write_bytes(31000, inode, 0, content.clone(), "test", &store, &objects)
                .await.opcode,
            abi::OP_RESULT_OK
        );
        let mut req = KestrelfsEvent::zeroed(abi::OP_FSYNC, 31001);
        req.payload[..8].copy_from_slice(&inode.to_le_bytes());
        assert_eq!(handle_sync(&req, &store, &objects).await.opcode, abi::OP_RESULT_OK);
        let global = KestrelfsEvent::zeroed(abi::OP_SYNC_FS, 31002);
        assert_eq!(handle_sync(&global, &store, &objects).await.opcode, abi::OP_RESULT_OK);

        let key = store.referenced_keys(inode).await.unwrap().remove(0);
        drop(store);
        drop(objects);
        let store: Arc<dyn MetaStore> = Arc::new(FileMetaStore::new(meta_path).await.unwrap());
        let objects: Arc<dyn ObjectStore> =
            Arc::new(LocalFsObjectStore::new(dir.path()).await.unwrap());
        assert_eq!(store.lookup(fs_model::ROOT_INODE, "sync-data").await.unwrap(), inode);
        assert_eq!(objects.get(&key).await.unwrap(), content);
        // A missing referenced object must never produce a false successful fsync.
        tokio::fs::remove_file(dir.path().join(&key)).await.unwrap();
        assert_eq!(handle_sync(&req, &store, &objects).await.error_code, -libc::EIO);
        assert_eq!(handle_sync(&global, &store, &objects).await.error_code, -libc::EIO);
    }

    #[tokio::test]
    async fn sync_opcodes_reject_nonzero_reserved_and_missing_inode() {
        let store: Arc<dyn MetaStore> = Arc::new(MemStore::new());
        let objects: Arc<dyn ObjectStore> = Arc::new(object_store::MemObjectStore::new());
        let mut req = KestrelfsEvent::zeroed(abi::OP_FSYNC, 1);
        req.payload[..8].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(handle_sync(&req, &store, &objects).await.error_code, -libc::ENOENT);
        req.payload[8] = 1;
        assert_eq!(handle_sync(&req, &store, &objects).await.error_code, -libc::EINVAL);
        let mut global = KestrelfsEvent::zeroed(abi::OP_SYNC_FS, 2);
        global.flags = 1;
        assert_eq!(handle_sync(&global, &store, &objects).await.error_code, -libc::EINVAL);
        global.flags = 0;
        assert_eq!(handle_sync(&global, &store, &objects).await.opcode, abi::OP_RESULT_OK);
    }

    /// Builds a raw `OP_GETATTR` request event.
    fn raw_getattr_req(req_id: u64, inode_id: u64) -> KestrelfsEvent {
        let mut event = KestrelfsEvent::zeroed(abi::OP_GETATTR, req_id);
        event.payload[0..8].copy_from_slice(&inode_id.to_le_bytes());
        event
    }

    fn raw_setattr_req(
        req_id: u64,
        inode_id: u64,
        valid: u32,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> KestrelfsEvent {
        let mut event = KestrelfsEvent::zeroed(abi::OP_SETATTR, req_id);
        event.payload[0..8].copy_from_slice(&inode_id.to_le_bytes());
        event.payload[8..12].copy_from_slice(&valid.to_le_bytes());
        event.payload[12..16].copy_from_slice(&mode.to_le_bytes());
        event.payload[16..20].copy_from_slice(&uid.to_le_bytes());
        event.payload[20..24].copy_from_slice(&gid.to_le_bytes());
        event
    }

    fn raw_setattr_time_req(
        req_id: u64,
        inode_id: u64,
        valid: u32,
        atime: u64,
        mtime: u64,
    ) -> KestrelfsEvent {
        let mut event = KestrelfsEvent::zeroed(abi::OP_SETATTR, req_id);
        event.payload[0..8].copy_from_slice(&inode_id.to_le_bytes());
        event.payload[8..12].copy_from_slice(&valid.to_le_bytes());
        event.payload[12..20].copy_from_slice(&atime.to_le_bytes());
        event.payload[20..28].copy_from_slice(&mtime.to_le_bytes());
        event
    }

    fn raw_getattr_times_req(req_id: u64, inode_id: u64) -> KestrelfsEvent {
        let mut event = KestrelfsEvent::zeroed(abi::OP_GETATTR_TIMES, req_id);
        event.payload[0..8].copy_from_slice(&inode_id.to_le_bytes());
        event
    }

    /// Shared test fixture: a fresh runtime + `MemStore`, exactly
    /// mirroring how `main()` constructs both.
    fn test_fixture() -> (
        tokio::runtime::Runtime,
        Arc<dyn MetaStore>,
        Arc<dyn ObjectStore>,
    ) {
        let runtime =
            tokio::runtime::Runtime::new().expect("runtime construction must not fail in tests");
        let store: Arc<dyn MetaStore> = Arc::new(MemStore::new());
        let object_store: Arc<dyn ObjectStore> = Arc::new(object_store::MemObjectStore::new());
        (runtime, store, object_store)
    }

    #[test]
    fn lookup_remote_txt_under_root_succeeds() {
        let (runtime, store, object_store) = test_fixture();
        let req = raw_lookup_req(1, fs_model::ROOT_INODE, "remote.txt");

        let resp = runtime.block_on(build_response(&req, &store, &object_store));

        assert_eq!(resp.opcode, abi::OP_RESULT_OK);
        assert_eq!(resp.req_id, 1);
        assert_eq!(resp.error_code, 0);

        let child_inode_id = u64::from_le_bytes(resp.payload[0..8].try_into().unwrap());
        let size = u64::from_le_bytes(resp.payload[8..16].try_into().unwrap());
        assert_eq!(child_inode_id, REMOTE_TXT_INODE);
        assert_eq!(size, 512);
    }

    #[test]
    fn lookup_unknown_name_returns_enoent() {
        let (runtime, store, object_store) = test_fixture();
        let req = raw_lookup_req(2, fs_model::ROOT_INODE, "does_not_exist.txt");

        let resp = runtime.block_on(build_response(&req, &store, &object_store));

        assert_eq!(resp.opcode, abi::OP_RESULT_ERROR);
        assert_eq!(resp.req_id, 2);
        assert_eq!(resp.error_code, -libc::ENOENT);
    }

    #[test]
    fn lookup_under_nonexistent_parent_returns_enoent() {
        let (runtime, store, object_store) = test_fixture();
        let req = raw_lookup_req(3, 9999, "anything");

        let resp = runtime.block_on(build_response(&req, &store, &object_store));

        assert_eq!(resp.opcode, abi::OP_RESULT_ERROR);
        assert_eq!(resp.error_code, -libc::ENOENT);
    }

    #[test]
    fn lookup_under_regular_file_returns_enotdir() {
        let (runtime, store, object_store) = test_fixture();
        // remote.txt (inode 2) is a regular file, not a directory.
        let req = raw_lookup_req(4, REMOTE_TXT_INODE, "anything");

        let resp = runtime.block_on(build_response(&req, &store, &object_store));

        assert_eq!(resp.opcode, abi::OP_RESULT_ERROR);
        assert_eq!(resp.error_code, -libc::ENOTDIR);
    }

    #[test]
    fn lookup_with_name_too_long_returns_enametoolong_without_querying_store() {
        let (runtime, store, object_store) = test_fixture();
        let mut req = KestrelfsEvent::zeroed(abi::OP_LOOKUP, 5);
        req.payload[0..8].copy_from_slice(&fs_model::ROOT_INODE.to_le_bytes());
        req.payload[8] = (abi::LOOKUP_NAME_MAX + 1) as u8;

        let resp = runtime.block_on(build_response(&req, &store, &object_store));

        assert_eq!(resp.opcode, abi::OP_RESULT_ERROR);
        assert_eq!(resp.error_code, -libc::ENAMETOOLONG);
    }

    #[test]
    fn getattr_root_succeeds() {
        let (runtime, store, object_store) = test_fixture();
        let req = raw_getattr_req(6, fs_model::ROOT_INODE);

        let resp = runtime.block_on(build_response(&req, &store, &object_store));

        assert_eq!(resp.opcode, abi::OP_RESULT_OK);
        assert_eq!(resp.error_code, 0);

        let mode = u32::from_le_bytes(resp.payload[8..12].try_into().unwrap());
        assert_eq!(mode & fs_model::S_IFDIR, fs_model::S_IFDIR);
    }

    #[test]
    fn getattr_remote_txt_succeeds() {
        let (runtime, store, object_store) = test_fixture();
        let req = raw_getattr_req(7, REMOTE_TXT_INODE);

        let resp = runtime.block_on(build_response(&req, &store, &object_store));

        assert_eq!(resp.opcode, abi::OP_RESULT_OK);
        assert_eq!(resp.error_code, 0);

        let size = u64::from_le_bytes(resp.payload[0..8].try_into().unwrap());
        let mode = u32::from_le_bytes(resp.payload[8..12].try_into().unwrap());
        assert_eq!(size, 512);
        assert_eq!(mode & fs_model::S_IFREG, fs_model::S_IFREG);
    }

    #[test]
    fn getattr_unknown_inode_returns_enoent() {
        let (runtime, store, object_store) = test_fixture();
        let req = raw_getattr_req(8, 9999);

        let resp = runtime.block_on(build_response(&req, &store, &object_store));

        assert_eq!(resp.opcode, abi::OP_RESULT_ERROR);
        assert_eq!(resp.error_code, -libc::ENOENT);
    }

    #[test]
    fn setattr_mode_preserves_type_and_getattr_observes_update() {
        let (runtime, store, object_store) = test_fixture();
        let req = raw_setattr_req(
            9,
            REMOTE_TXT_INODE,
            abi::SETATTR_MODE,
            fs_model::S_IFDIR | 0o6751,
            0,
            0,
        );

        let resp = runtime.block_on(build_response(&req, &store, &object_store));
        assert_eq!(resp.opcode, abi::OP_RESULT_OK);
        assert_eq!(
            u32::from_le_bytes(resp.payload[0..4].try_into().unwrap()),
            fs_model::S_IFREG | 0o6751
        );

        let getattr = raw_getattr_req(10, REMOTE_TXT_INODE);
        let resp = runtime.block_on(build_response(&getattr, &store, &object_store));
        assert_eq!(resp.opcode, abi::OP_RESULT_OK);
        assert_eq!(
            u32::from_le_bytes(resp.payload[8..12].try_into().unwrap()),
            fs_model::S_IFREG | 0o6751
        );
    }

    #[test]
    fn setattr_uid_gid_combination_updates_getattr_and_response() {
        let (runtime, store, object_store) = test_fixture();
        let req = raw_setattr_req(
            13,
            REMOTE_TXT_INODE,
            abi::SETATTR_UID | abi::SETATTR_GID,
            0,
            1234,
            2345,
        );

        let resp = runtime.block_on(build_response(&req, &store, &object_store));
        assert_eq!(resp.opcode, abi::OP_RESULT_OK);
        assert_eq!(u32::from_le_bytes(resp.payload[4..8].try_into().unwrap()), 1234);
        assert_eq!(u32::from_le_bytes(resp.payload[8..12].try_into().unwrap()), 2345);

        let getattr = raw_getattr_req(14, REMOTE_TXT_INODE);
        let resp = runtime.block_on(build_response(&getattr, &store, &object_store));
        assert_eq!(u32::from_le_bytes(resp.payload[12..16].try_into().unwrap()), 1234);
        assert_eq!(u32::from_le_bytes(resp.payload[16..20].try_into().unwrap()), 2345);
    }

    #[test]
    fn setattr_times_are_atomic_and_getattr_times_observes_update() {
        let (runtime, store, object_store) = test_fixture();
        let req = raw_setattr_time_req(
            15,
            REMOTE_TXT_INODE,
            abi::SETATTR_ATIME | abi::SETATTR_MTIME,
            1_577_836_800,
            1_577_836_801,
        );

        let resp = runtime.block_on(build_response(&req, &store, &object_store));
        assert_eq!(resp.opcode, abi::OP_RESULT_OK);
        assert_eq!(
            u64::from_le_bytes(resp.payload[0..8].try_into().unwrap()),
            1_577_836_800
        );
        assert_eq!(
            u64::from_le_bytes(resp.payload[8..16].try_into().unwrap()),
            1_577_836_801
        );

        let getattr = raw_getattr_times_req(16, REMOTE_TXT_INODE);
        let resp = runtime.block_on(build_response(&getattr, &store, &object_store));
        assert_eq!(resp.opcode, abi::OP_RESULT_OK);
        assert_eq!(
            u64::from_le_bytes(resp.payload[0..8].try_into().unwrap()),
            1_577_836_800
        );
        assert_eq!(
            u64::from_le_bytes(resp.payload[8..16].try_into().unwrap()),
            1_577_836_801
        );
    }

    #[test]
    fn setattr_rejects_unsupported_mask_and_nonzero_reserved_bytes() {
        let (runtime, store, object_store) = test_fixture();
        let unsupported = raw_setattr_req(11, REMOTE_TXT_INODE, 1 << 31, 0o600, 0, 0);
        let resp = runtime.block_on(build_response(&unsupported, &store, &object_store));
        assert_eq!(resp.opcode, abi::OP_RESULT_ERROR);
        assert_eq!(resp.error_code, -libc::EINVAL);

        let mut reserved =
            raw_setattr_req(12, REMOTE_TXT_INODE, abi::SETATTR_MODE, 0o600, 0, 0);
        reserved.payload[24] = 1;
        let resp = runtime.block_on(build_response(&reserved, &store, &object_store));
        assert_eq!(resp.opcode, abi::OP_RESULT_ERROR);
        assert_eq!(resp.error_code, -libc::EINVAL);

        let mixed = raw_setattr_time_req(
            17,
            REMOTE_TXT_INODE,
            abi::SETATTR_MODE | abi::SETATTR_ATIME,
            1_577_836_800,
            1_577_836_801,
        );
        let resp = runtime.block_on(build_response(&mixed, &store, &object_store));
        assert_eq!(resp.opcode, abi::OP_RESULT_ERROR);
        assert_eq!(resp.error_code, -libc::EINVAL);

        let mut time_reserved = raw_setattr_time_req(
            18,
            REMOTE_TXT_INODE,
            abi::SETATTR_ATIME,
            1_577_836_800,
            0,
        );
        time_reserved.payload[28] = 1;
        let resp = runtime.block_on(build_response(&time_reserved, &store, &object_store));
        assert_eq!(resp.opcode, abi::OP_RESULT_ERROR);
        assert_eq!(resp.error_code, -libc::EINVAL);
    }

    #[test]
    fn read_chunk_returns_seeded_data_from_object_store() {
        // Phase 3 step 3: READ_CHUNK now fetches real data from
        // MetaStore (slice metadata) + ObjectStore (block bytes),
        // replacing the old synthetic "KestrelFS remote chunk @ offset=..."
        // stub. This test verifies the end-to-end path for remote.txt
        // (inode 3), which MemStore::new() seeds with one Slice covering
        // bytes 0..512, and test_fixture() seeds the corresponding block
        // in MemObjectStore.
        let (runtime, store, object_store) = test_fixture();

        // Seed the block so the test has data to read
        runtime.block_on(async {
            seed_remote_txt_block(&object_store)
                .await
                .expect("seed failed");
        });

        // Construct OP_READ_CHUNK request for inode=3, offset=0, count=32
        // (the ABI_VERSION 2 layout: inode_id@0, offset@8, count@16)
        let mut req = KestrelfsEvent::zeroed(abi::OP_READ_CHUNK, 10);
        req.payload[0..8].copy_from_slice(&meta::REMOTE_TXT_INODE.to_le_bytes());
        req.payload[8..16].copy_from_slice(&0u64.to_le_bytes());
        req.payload[16..20].copy_from_slice(&32u32.to_le_bytes());

        let resp = runtime.block_on(build_response(&req, &store, &object_store));

        assert_eq!(resp.opcode, abi::OP_RESULT_OK);
        // The seeded pattern is "Phase3-seed-data! " repeated - verify
        // the response payload starts with that pattern
        let data = &resp.payload[..32];
        let expected_start = b"Phase3-seed-data! Phase3-seed-da";
        assert_eq!(data, expected_start);
    }

    #[test]
    fn nop_still_returns_ok() {
        let (runtime, store, object_store) = test_fixture();
        let req = KestrelfsEvent::zeroed(abi::OP_NOP, 10);

        let resp = runtime.block_on(build_response(&req, &store, &object_store));

        assert_eq!(resp.opcode, abi::OP_RESULT_OK);
        assert_eq!(resp.req_id, 10);
    }

    #[tokio::test]
    async fn read_from_slices_returns_zeros_when_no_slices_exist() {
        let store: Arc<dyn MetaStore> = Arc::new(MemStore::new());
        let object_store: Arc<dyn ObjectStore> = Arc::new(object_store::MemObjectStore::new());

        // Create a new file with size 4096 but no slices (sparse file)
        let inode_id = store
            .create(fs_model::ROOT_INODE, "sparse.dat", fs_model::S_IFREG | 0o666)
            .await
            .expect("create must succeed");

        // Set size to 4096 but don't write any slices
        store
            .truncate(inode_id, 4096)
            .await
            .expect("truncate must succeed");

        // Read from middle of file where no slices exist (sparse hole)
        let data = read_from_slices(inode_id, 2048, 16, &store, &object_store)
            .await
            .expect("read_from_slices must not fail on sparse holes");

        assert_eq!(data.len(), 16);
        assert_eq!(data, vec![0u8; 16]);
    }

    #[tokio::test]
    async fn read_from_slices_returns_data_from_single_slice() {
        let store: Arc<dyn MetaStore> = Arc::new(MemStore::new());
        let object_store: Arc<dyn ObjectStore> = Arc::new(object_store::MemObjectStore::new());

        // Seed the existing slice's block (remote.txt chunk 0)
        seed_remote_txt_block(&object_store)
            .await
            .expect("seed failed");

        // Read bytes 10..26 from chunk 0 (where the seed data exists)
        let data = read_from_slices(meta::REMOTE_TXT_INODE, 10, 16, &store, &object_store)
            .await
            .expect("read must succeed");

        assert_eq!(data.len(), 16);
        // "Phase3-seed-data! " repeated (18 bytes), bytes [10..26] = "d-data! Phase3-s"
        assert_eq!(&data, b"d-data! Phase3-s");
    }

    #[tokio::test]
    async fn read_from_slices_rejects_object_length_mismatch() {
        let mem_store = MemStore::new();
        let object_store: Arc<dyn ObjectStore> = Arc::new(object_store::MemObjectStore::new());
        let inode = 98;
        mem_store
            .insert_inode_for_test(inode, fs_model::Inode::new_file(inode, 4, 1))
            .await;
        let slice = Slice {
            chunk_index: 0,
            slice_id: uuid::Uuid::new_v4(),
            chunk_offset: 0,
            length: 4,
            written_at: 1,
        };
        let key = slice.block_key(0);
        object_store.put(key, vec![1, 2, 3]).await.unwrap();
        mem_store.append_slice(inode, slice).await.unwrap();
        let store: Arc<dyn MetaStore> = Arc::new(mem_store);

        let error = read_from_slices(inode, 0, 4, &store, &object_store)
            .await
            .unwrap_err();
        assert!(error.contains("object integrity error"), "{error}");
    }

    #[tokio::test]
    async fn read_from_slices_prefers_newer_written_at_on_overlap() {
        use fs_model::Slice;

        let mem_store = MemStore::new();
        let object_store: Arc<dyn ObjectStore> = Arc::new(object_store::MemObjectStore::new());

        // Manually inject two overlapping slices into MemStore for a test inode
        let test_inode = 99u64;
        let test_size = 32u64;

        // Add inode 99
        mem_store
            .insert_inode_for_test(
                test_inode,
                fs_model::Inode::new_file(test_inode, test_size, 1000),
            )
            .await;

        // Create two slices both covering byte 10 in chunk 0
        let slice_old = Slice {
            chunk_index: 0,
            slice_id: uuid::Uuid::from_u128(1),
            chunk_offset: 0,
            length: 20,
            written_at: 1000,
        };

        let slice_new = Slice {
            chunk_index: 0,
            slice_id: uuid::Uuid::from_u128(2),
            chunk_offset: 5,
            length: 10,
            written_at: 2000, // newer
        };

        // Write block data: old slice = all 'A', new slice = all 'B'
        let old_block_key = slice_old.block_key(0);
        let new_block_key = slice_new.block_key(0);
        object_store
            .put(old_block_key, vec![b'A'; 20])
            .await
            .unwrap();
        object_store
            .put(new_block_key, vec![b'B'; 10])
            .await
            .unwrap();

        // Insert both slices (old first, then new - order shouldn't matter)
        mem_store
            .insert_slices_for_test(test_inode, 0, vec![slice_old, slice_new])
            .await;

        let store: Arc<dyn MetaStore> = Arc::new(mem_store);

        // Read byte 10: covered by both slices, but slice_new wins (written_at=2000 > 1000)
        // slice_old: covers chunk bytes [0..20], so byte 10 = block[10] = 'A'
        // slice_new: covers chunk bytes [5..15], so byte 10 = block[10-5=5] = 'B'
        let data = read_from_slices(test_inode, 10, 1, &store, &object_store)
            .await
            .expect("read must succeed");

        assert_eq!(data.len(), 1);
        assert_eq!(data[0], b'B', "newer slice (written_at=2000) must win");
    }

    #[tokio::test]
    async fn debug_block1_direct_access() {
        use fs_model::Slice;

        let mem_store = MemStore::new();
        let object_store: Arc<dyn ObjectStore> = Arc::new(object_store::MemObjectStore::new());

        let test_inode = 101u64;
        mem_store
            .insert_inode_for_test(
                test_inode,
                fs_model::Inode::new_file(test_inode, 1024, 1000),
            )
            .await;

        // Single slice starting at chunk offset 512, length 8 (entirely in "block 1" territory)
        let slice = Slice {
            chunk_index: 0,
            slice_id: uuid::Uuid::from_u128(99),
            chunk_offset: 512, // starts at block boundary
            length: 8,
            written_at: 1000,
        };

        // This slice's data starts at chunk byte 512, which is block_idx=1, byte_in_block=0
        let block_key = slice.block_key(0); // block 0 of this slice (which covers chunk bytes 512-519)
        println!("Block key: {}", block_key);
        object_store
            .put(block_key.clone(), vec![b'Z'; 8])
            .await
            .unwrap();

        // Verify it's stored
        let retrieved = object_store.get(&block_key).await.unwrap();
        assert_eq!(retrieved, vec![b'Z'; 8]);

        mem_store
            .insert_slices_for_test(test_inode, 0, vec![slice])
            .await;
        let store: Arc<dyn MetaStore> = Arc::new(mem_store);

        // Read chunk bytes [512..520]
        let data = read_from_slices(test_inode, 512, 8, &store, &object_store)
            .await
            .expect("read must succeed");

        println!("Read data: {:?}", data);
        assert_eq!(data, vec![b'Z'; 8]);
    }

    #[tokio::test]
    async fn handle_write_chunk_creates_slice_and_stores_data() {
        let mem_store = MemStore::new();
        let object_store: Arc<dyn ObjectStore> = Arc::new(object_store::MemObjectStore::new());

        // Create a file to write to
        let inode_id = mem_store
            .create(fs_model::ROOT_INODE, "test.txt", fs_model::S_IFREG | 0o644)
            .await
            .expect("create must succeed");

        let store: Arc<dyn MetaStore> = Arc::new(mem_store);

        // Build a WRITE_CHUNK request: write "hello" at offset 0
        let mut event = KestrelfsEvent {
            seq: 0,
            opcode: abi::OP_WRITE_CHUNK,
            flags: 0,
            req_id: 1,
            error_code: 0,
            _reserved0: 0,
            payload: [0u8; abi::EVENT_PAYLOAD_SIZE],
        };
        event.payload[0..8].copy_from_slice(&inode_id.to_le_bytes());
        event.payload[8..16].copy_from_slice(&0u64.to_le_bytes()); // offset=0
        event.payload[16..20].copy_from_slice(&5u32.to_le_bytes()); // count=5
        event.payload[20..25].copy_from_slice(b"hello");

        let resp = handle_write_chunk(&event, &store, &object_store).await;
        assert_eq!(resp.opcode, abi::OP_RESULT_OK);

        // Verify the data is readable back
        let read_data = read_from_slices(inode_id, 0, 5, &store, &object_store)
            .await
            .expect("read must succeed");
        assert_eq!(&read_data, b"hello");

        // Verify inode size was updated
        let inode = store.getattr(inode_id).await.expect("getattr must succeed");
        assert_eq!(inode.size, 5);
    }

    #[tokio::test]
    async fn handle_write_chunk_to_nonexistent_inode_returns_enoent() {
        let store: Arc<dyn MetaStore> = Arc::new(MemStore::new());
        let object_store: Arc<dyn ObjectStore> = Arc::new(object_store::MemObjectStore::new());

        let mut event = KestrelfsEvent {
            seq: 0,
            opcode: abi::OP_WRITE_CHUNK,
            flags: 0,
            req_id: 1,
            error_code: 0,
            _reserved0: 0,
            payload: [0u8; abi::EVENT_PAYLOAD_SIZE],
        };
        event.payload[0..8].copy_from_slice(&999u64.to_le_bytes()); // nonexistent inode
        event.payload[8..16].copy_from_slice(&0u64.to_le_bytes());
        event.payload[16..20].copy_from_slice(&5u32.to_le_bytes());
        event.payload[20..25].copy_from_slice(b"hello");

        let resp = handle_write_chunk(&event, &store, &object_store).await;
        assert_eq!(resp.opcode, abi::OP_RESULT_ERROR);
        assert_eq!(resp.error_code, -libc::ENOENT);
    }

    #[tokio::test]
    async fn handle_write_chunk_cow_preserves_old_data() {
        let mem_store = MemStore::new();
        let object_store: Arc<dyn ObjectStore> = Arc::new(object_store::MemObjectStore::new());

        let inode_id = mem_store
            .create(fs_model::ROOT_INODE, "test.txt", fs_model::S_IFREG | 0o644)
            .await
            .expect("create must succeed");

        let store: Arc<dyn MetaStore> = Arc::new(mem_store);

        // Write "AAAAA" at offset 0
        let mut event1 = KestrelfsEvent {
            seq: 0,
            opcode: abi::OP_WRITE_CHUNK,
            flags: 0,
            req_id: 1,
            error_code: 0,
            _reserved0: 0,
            payload: [0u8; abi::EVENT_PAYLOAD_SIZE],
        };
        event1.payload[0..8].copy_from_slice(&inode_id.to_le_bytes());
        event1.payload[8..16].copy_from_slice(&0u64.to_le_bytes());
        event1.payload[16..20].copy_from_slice(&5u32.to_le_bytes());
        event1.payload[20..25].copy_from_slice(b"AAAAA");
        handle_write_chunk(&event1, &store, &object_store).await;

        // Write "BB" at offset 2 (overlapping)
        let mut event2 = KestrelfsEvent {
            seq: 0,
            opcode: abi::OP_WRITE_CHUNK,
            flags: 0,
            req_id: 2,
            error_code: 0,
            _reserved0: 0,
            payload: [0u8; abi::EVENT_PAYLOAD_SIZE],
        };
        event2.payload[0..8].copy_from_slice(&inode_id.to_le_bytes());
        event2.payload[8..16].copy_from_slice(&2u64.to_le_bytes());
        event2.payload[16..20].copy_from_slice(&2u32.to_le_bytes());
        event2.payload[20..22].copy_from_slice(b"BB");
        handle_write_chunk(&event2, &store, &object_store).await;

        // Read back: should get "AABBA" (newer write wins on overlap)
        let data = read_from_slices(inode_id, 0, 5, &store, &object_store)
            .await
            .expect("read must succeed");
        assert_eq!(&data, b"AABBA");
    }

    #[tokio::test]
    async fn truncate_shrinks_file_and_clamps_reads() {
        let mem_store = MemStore::new();
        let object_store: Arc<dyn ObjectStore> = Arc::new(object_store::MemObjectStore::new());

        let inode_id = mem_store
            .create(fs_model::ROOT_INODE, "test.txt", fs_model::S_IFREG | 0o644)
            .await
            .expect("create must succeed");

        let store: Arc<dyn MetaStore> = Arc::new(mem_store);

        // Write 12 bytes "VERYLONGTEXT"
        let mut event = KestrelfsEvent {
            seq: 0,
            opcode: abi::OP_WRITE_CHUNK,
            flags: 0,
            req_id: 1,
            error_code: 0,
            _reserved0: 0,
            payload: [0u8; abi::EVENT_PAYLOAD_SIZE],
        };
        event.payload[0..8].copy_from_slice(&inode_id.to_le_bytes());
        event.payload[8..16].copy_from_slice(&0u64.to_le_bytes());
        event.payload[16..20].copy_from_slice(&12u32.to_le_bytes());
        event.payload[20..32].copy_from_slice(b"VERYLONGTEXT");
        handle_write_chunk(&event, &store, &object_store).await;

        // Verify size is 12
        let inode = store.getattr(inode_id).await.expect("getattr must succeed");
        assert_eq!(inode.size, 12);

        // Truncate to 5 bytes
        store
            .truncate(inode_id, 5)
            .await
            .expect("truncate must succeed");

        // Verify new size
        let inode = store.getattr(inode_id).await.expect("getattr must succeed");
        assert_eq!(inode.size, 5);

        // Read should return only 5 bytes (no old tail)
        let data = read_from_slices(inode_id, 0, 12, &store, &object_store)
            .await
            .expect("read must succeed");
        assert_eq!(data.len(), 5);
        assert_eq!(&data, b"VERYL");

        // Read beyond EOF returns empty
        let data = read_from_slices(inode_id, 10, 5, &store, &object_store)
            .await
            .expect("read must succeed");
        assert_eq!(data.len(), 0);
    }

    #[tokio::test]
    async fn truncate_to_zero_empties_file() {
        let mem_store = MemStore::new();
        let object_store: Arc<dyn ObjectStore> = Arc::new(object_store::MemObjectStore::new());

        let inode_id = mem_store
            .create(fs_model::ROOT_INODE, "test.txt", fs_model::S_IFREG | 0o644)
            .await
            .expect("create must succeed");

        let store: Arc<dyn MetaStore> = Arc::new(mem_store);

        // Write some data
        let mut event = KestrelfsEvent {
            seq: 0,
            opcode: abi::OP_WRITE_CHUNK,
            flags: 0,
            req_id: 1,
            error_code: 0,
            _reserved0: 0,
            payload: [0u8; abi::EVENT_PAYLOAD_SIZE],
        };
        event.payload[0..8].copy_from_slice(&inode_id.to_le_bytes());
        event.payload[8..16].copy_from_slice(&0u64.to_le_bytes());
        event.payload[16..20].copy_from_slice(&5u32.to_le_bytes());
        event.payload[20..25].copy_from_slice(b"HELLO");
        handle_write_chunk(&event, &store, &object_store).await;

        // Truncate to 0
        store
            .truncate(inode_id, 0)
            .await
            .expect("truncate must succeed");

        let inode = store.getattr(inode_id).await.expect("getattr must succeed");
        assert_eq!(inode.size, 0);

        // Read returns empty
        let data = read_from_slices(inode_id, 0, 10, &store, &object_store)
            .await
            .expect("read must succeed");
        assert_eq!(data.len(), 0);
    }

    #[tokio::test]
    async fn write_after_truncate_does_not_shrink_size() {
        let mem_store = MemStore::new();
        let object_store: Arc<dyn ObjectStore> = Arc::new(object_store::MemObjectStore::new());

        let inode_id = mem_store
            .create(fs_model::ROOT_INODE, "test.txt", fs_model::S_IFREG | 0o644)
            .await
            .expect("create must succeed");

        let store: Arc<dyn MetaStore> = Arc::new(mem_store);

        // Truncate to 100 (sparse file)
        store
            .truncate(inode_id, 100)
            .await
            .expect("truncate must succeed");

        // Write 3 bytes at offset 50 (middle of sparse region)
        let mut event = KestrelfsEvent {
            seq: 0,
            opcode: abi::OP_WRITE_CHUNK,
            flags: 0,
            req_id: 1,
            error_code: 0,
            _reserved0: 0,
            payload: [0u8; abi::EVENT_PAYLOAD_SIZE],
        };
        event.payload[0..8].copy_from_slice(&inode_id.to_le_bytes());
        event.payload[8..16].copy_from_slice(&50u64.to_le_bytes());
        event.payload[16..20].copy_from_slice(&3u32.to_le_bytes());
        event.payload[20..23].copy_from_slice(b"ABC");
        handle_write_chunk(&event, &store, &object_store).await;

        // Size should remain 100 (write doesn't shrink)
        let inode = store.getattr(inode_id).await.expect("getattr must succeed");
        assert_eq!(inode.size, 100);

        // Read at offset 50 gets "ABC"
        let data = read_from_slices(inode_id, 50, 3, &store, &object_store)
            .await
            .expect("read must succeed");
        assert_eq!(&data, b"ABC");

        // Read before write position gets zeros (sparse)
        let data = read_from_slices(inode_id, 0, 10, &store, &object_store)
            .await
            .expect("read must succeed");
        assert_eq!(data, vec![0u8; 10]);
    }

    /// Builds a raw `OP_MKDIR` request event.
    fn raw_mkdir_req(req_id: u64, parent_inode: u64, mode: u32, name: &str) -> KestrelfsEvent {
        let mut event = KestrelfsEvent::zeroed(abi::OP_MKDIR, req_id);
        event.payload[0..8].copy_from_slice(&parent_inode.to_le_bytes());
        event.payload[8..12].copy_from_slice(&mode.to_le_bytes());
        let name_bytes = name.as_bytes();
        event.payload[12..12 + name_bytes.len()].copy_from_slice(name_bytes);
        event.payload[12 + name_bytes.len()] = 0; // NUL terminator
        event
    }

    /// Builds a raw `OP_UNLINK` request event.
    fn raw_unlink_req(req_id: u64, parent_inode: u64, name: &str) -> KestrelfsEvent {
        let mut event = KestrelfsEvent::zeroed(abi::OP_UNLINK, req_id);
        event.payload[0..8].copy_from_slice(&parent_inode.to_le_bytes());
        let name_bytes = name.as_bytes();
        event.payload[8..8 + name_bytes.len()].copy_from_slice(name_bytes);
        event.payload[8 + name_bytes.len()] = 0; // NUL terminator
        event
    }

    #[test]
    fn mkdir_creates_new_directory() {
        let (runtime, store, object_store) = test_fixture();
        let req = raw_mkdir_req(100, fs_model::ROOT_INODE, 0o755, "testdir");

        let resp = runtime.block_on(build_response(&req, &store, &object_store));

        assert_eq!(resp.opcode, abi::OP_RESULT_OK);
        assert_eq!(resp.req_id, 100);
        assert_eq!(resp.error_code, 0);

        let new_dir_inode = u64::from_le_bytes(resp.payload[0..8].try_into().unwrap());
        assert!(new_dir_inode > 0);

        // Verify directory exists via lookup
        let lookup_req = raw_lookup_req(101, fs_model::ROOT_INODE, "testdir");
        let lookup_resp = runtime.block_on(build_response(&lookup_req, &store, &object_store));
        assert_eq!(lookup_resp.opcode, abi::OP_RESULT_OK);
        let found_inode = u64::from_le_bytes(lookup_resp.payload[0..8].try_into().unwrap());
        assert_eq!(found_inode, new_dir_inode);
    }

    #[test]
    fn mkdir_duplicate_name_returns_eexist() {
        let (runtime, store, object_store) = test_fixture();
        
        // Create first directory
        let req1 = raw_mkdir_req(102, fs_model::ROOT_INODE, 0o755, "duplicate");
        let resp1 = runtime.block_on(build_response(&req1, &store, &object_store));
        assert_eq!(resp1.opcode, abi::OP_RESULT_OK);

        // Try to create again with same name
        let req2 = raw_mkdir_req(103, fs_model::ROOT_INODE, 0o755, "duplicate");
        let resp2 = runtime.block_on(build_response(&req2, &store, &object_store));
        assert_eq!(resp2.opcode, abi::OP_RESULT_ERROR);
        assert_eq!(resp2.error_code, -libc::EEXIST);
    }

    #[test]
    fn unlink_removes_file() {
        let (runtime, store, object_store) = test_fixture();
        
        // Create a file first
        let create_req = raw_create_req(104, fs_model::ROOT_INODE, 0o644, "tempfile");
        let create_resp = runtime.block_on(build_response(&create_req, &store, &object_store));
        assert_eq!(create_resp.opcode, abi::OP_RESULT_OK);

        // Unlink the file
        let unlink_req = raw_unlink_req(105, fs_model::ROOT_INODE, "tempfile");
        let unlink_resp = runtime.block_on(build_response(&unlink_req, &store, &object_store));
        assert_eq!(unlink_resp.opcode, abi::OP_RESULT_OK);

        // Verify file no longer exists
        let lookup_req = raw_lookup_req(106, fs_model::ROOT_INODE, "tempfile");
        let lookup_resp = runtime.block_on(build_response(&lookup_req, &store, &object_store));
        assert_eq!(lookup_resp.opcode, abi::OP_RESULT_ERROR);
        assert_eq!(lookup_resp.error_code, -libc::ENOENT);
    }

    #[test]
    fn unlink_empty_directory_succeeds() {
        let (runtime, store, object_store) = test_fixture();
        
        // Create an empty directory
        let mkdir_req = raw_mkdir_req(107, fs_model::ROOT_INODE, 0o755, "emptydir");
        let mkdir_resp = runtime.block_on(build_response(&mkdir_req, &store, &object_store));
        assert_eq!(mkdir_resp.opcode, abi::OP_RESULT_OK);

        // Unlink (rmdir) the empty directory
        let unlink_req = raw_unlink_req(108, fs_model::ROOT_INODE, "emptydir");
        let unlink_resp = runtime.block_on(build_response(&unlink_req, &store, &object_store));
        assert_eq!(unlink_resp.opcode, abi::OP_RESULT_OK);

        // Verify directory no longer exists
        let lookup_req = raw_lookup_req(109, fs_model::ROOT_INODE, "emptydir");
        let lookup_resp = runtime.block_on(build_response(&lookup_req, &store, &object_store));
        assert_eq!(lookup_resp.opcode, abi::OP_RESULT_ERROR);
        assert_eq!(lookup_resp.error_code, -libc::ENOENT);
    }

    #[test]
    fn unlink_nonempty_directory_returns_enotempty() {
        let (runtime, store, object_store) = test_fixture();
        
        // Create a directory
        let mkdir_req = raw_mkdir_req(110, fs_model::ROOT_INODE, 0o755, "nonempty");
        let mkdir_resp = runtime.block_on(build_response(&mkdir_req, &store, &object_store));
        assert_eq!(mkdir_resp.opcode, abi::OP_RESULT_OK);
        let dir_inode = u64::from_le_bytes(mkdir_resp.payload[0..8].try_into().unwrap());

        // Create a file inside the directory
        let create_req = raw_create_req(111, dir_inode, 0o644, "child.txt");
        let create_resp = runtime.block_on(build_response(&create_req, &store, &object_store));
        assert_eq!(create_resp.opcode, abi::OP_RESULT_OK);

        // Try to unlink the non-empty directory
        let unlink_req = raw_unlink_req(112, fs_model::ROOT_INODE, "nonempty");
        let unlink_resp = runtime.block_on(build_response(&unlink_req, &store, &object_store));
        assert_eq!(unlink_resp.opcode, abi::OP_RESULT_ERROR);
        assert_eq!(unlink_resp.error_code, -libc::ENOTEMPTY);
    }

    #[test]
    fn mkdir_create_unlink_workflow_with_persistence() {
        let (runtime, store, object_store) = test_fixture();

        // 1. Create directory
        let mkdir_req = raw_mkdir_req(200, fs_model::ROOT_INODE, 0o755, "persist_dir");
        let mkdir_resp = runtime.block_on(build_response(&mkdir_req, &store, &object_store));
        assert_eq!(mkdir_resp.opcode, abi::OP_RESULT_OK);
        let dir_inode = u64::from_le_bytes(mkdir_resp.payload[0..8].try_into().unwrap());

        // 2. Create file inside directory
        let create_req = raw_create_req(201, dir_inode, 0o644, "persist_file.txt");
        let create_resp = runtime.block_on(build_response(&create_req, &store, &object_store));
        assert_eq!(create_resp.opcode, abi::OP_RESULT_OK);

        // 3. Unlink the file
        let unlink_req = raw_unlink_req(202, dir_inode, "persist_file.txt");
        let unlink_resp = runtime.block_on(build_response(&unlink_req, &store, &object_store));
        assert_eq!(unlink_resp.opcode, abi::OP_RESULT_OK);

        // 4. Verify file is gone
        let lookup_file = raw_lookup_req(203, dir_inode, "persist_file.txt");
        let lookup_file_resp = runtime.block_on(build_response(&lookup_file, &store, &object_store));
        assert_eq!(lookup_file_resp.opcode, abi::OP_RESULT_ERROR);
        assert_eq!(lookup_file_resp.error_code, -libc::ENOENT);

        // 5. Unlink the empty directory
        let unlink_dir_req = raw_unlink_req(204, fs_model::ROOT_INODE, "persist_dir");
        let unlink_dir_resp = runtime.block_on(build_response(&unlink_dir_req, &store, &object_store));
        assert_eq!(unlink_dir_resp.opcode, abi::OP_RESULT_OK);

        // 6. Verify directory is gone
        let lookup_dir = raw_lookup_req(205, fs_model::ROOT_INODE, "persist_dir");
        let lookup_dir_resp = runtime.block_on(build_response(&lookup_dir, &store, &object_store));
        assert_eq!(lookup_dir_resp.opcode, abi::OP_RESULT_ERROR);
        assert_eq!(lookup_dir_resp.error_code, -libc::ENOENT);
    }

    /// Helper to build raw CREATE request (already exists in tests, but adding for clarity)
    fn raw_create_req(req_id: u64, parent_inode: u64, mode: u32, name: &str) -> KestrelfsEvent {
        let mut event = KestrelfsEvent::zeroed(abi::OP_CREATE, req_id);
        event.payload[0..8].copy_from_slice(&parent_inode.to_le_bytes());
        event.payload[8..12].copy_from_slice(&mode.to_le_bytes());
        let name_bytes = name.as_bytes();
        event.payload[12..12 + name_bytes.len()].copy_from_slice(name_bytes);
        event.payload[12 + name_bytes.len()] = 0;
        event
    }

    /// Helper to build raw RENAME request
    fn raw_rename_req(
        req_id: u64,
        old_parent: u64,
        old_name: &str,
        new_parent: u64,
        new_name: &str,
    ) -> KestrelfsEvent {
        let mut event = KestrelfsEvent::zeroed(abi::OP_RENAME, req_id);
        let old_name_bytes = old_name.as_bytes();
        let new_name_bytes = new_name.as_bytes();
        
        event.payload[0..8].copy_from_slice(&old_parent.to_le_bytes());
        event.payload[8..16].copy_from_slice(&new_parent.to_le_bytes());
        event.payload[16] = old_name_bytes.len() as u8;
        event.payload[17] = new_name_bytes.len() as u8;
        event.payload[18..18 + old_name_bytes.len()].copy_from_slice(old_name_bytes);
        event.payload[25..25 + new_name_bytes.len()].copy_from_slice(new_name_bytes);
        event
    }

    /// Builds an ABI v9 rename request and lays old_name then new_name at the
    /// front of the shared data bounce buffer.
    fn raw_rename_data_req(
        req_id: u64,
        old_parent: u64,
        old_name: &str,
        new_parent: u64,
        new_name: &str,
        data_buffer: &mut [u8; abi::DATA_BUFFER_SIZE],
    ) -> KestrelfsEvent {
        raw_rename_data_req_with_flags(
            req_id,
            old_parent,
            old_name,
            new_parent,
            new_name,
            0,
            data_buffer,
        )
    }

    fn raw_rename_data_req_with_flags(
        req_id: u64,
        old_parent: u64,
        old_name: &str,
        new_parent: u64,
        new_name: &str,
        flags: u32,
        data_buffer: &mut [u8; abi::DATA_BUFFER_SIZE],
    ) -> KestrelfsEvent {
        let old_name = old_name.as_bytes();
        let new_name = new_name.as_bytes();
        assert!(old_name.len() <= abi::RENAME_DATA_NAME_MAX);
        assert!(new_name.len() <= abi::RENAME_DATA_NAME_MAX);
        assert!(old_name.len() + new_name.len() <= data_buffer.len());

        data_buffer[..old_name.len()].copy_from_slice(old_name);
        data_buffer[old_name.len()..old_name.len() + new_name.len()]
            .copy_from_slice(new_name);

        let mut event = KestrelfsEvent::zeroed(abi::OP_RENAME_DATA, req_id);
        event.payload[0..8].copy_from_slice(&old_parent.to_le_bytes());
        event.payload[8..16].copy_from_slice(&new_parent.to_le_bytes());
        event.payload[16..18].copy_from_slice(&(old_name.len() as u16).to_le_bytes());
        event.payload[18..20].copy_from_slice(&(new_name.len() as u16).to_le_bytes());
        event.payload[20..24].copy_from_slice(&flags.to_le_bytes());
        event
    }

    fn raw_name_data_req(
        opcode: u32,
        req_id: u64,
        parent_inode: u64,
        mode: u32,
        name: &str,
        data_buffer: &mut [u8; abi::DATA_BUFFER_SIZE],
    ) -> KestrelfsEvent {
        let name = name.as_bytes();
        assert!(!name.is_empty());
        assert!(name.len() <= abi::NAME_DATA_MAX);
        data_buffer[..name.len()].copy_from_slice(name);

        let mut event = KestrelfsEvent::zeroed(opcode, req_id);
        event.payload[0..8].copy_from_slice(&parent_inode.to_le_bytes());
        event.payload[8..10].copy_from_slice(&(name.len() as u16).to_le_bytes());
        event.payload[12..16].copy_from_slice(&mode.to_le_bytes());
        event
    }

    fn raw_readdir_data_req(req_id: u64, dir_inode: u64, offset: u32) -> KestrelfsEvent {
        let mut event = KestrelfsEvent::zeroed(abi::OP_READDIR_DATA, req_id);
        event.payload[0..8].copy_from_slice(&dir_inode.to_le_bytes());
        event.payload[8..12].copy_from_slice(&offset.to_le_bytes());
        event
    }

    fn raw_link_data_req(
        req_id: u64,
        parent_inode: u64,
        inode_id: u64,
        name: &str,
        data_buffer: &mut [u8; abi::DATA_BUFFER_SIZE],
    ) -> KestrelfsEvent {
        let name = name.as_bytes();
        assert!(!name.is_empty());
        assert!(name.len() <= abi::NAME_DATA_MAX);
        data_buffer[..name.len()].copy_from_slice(name);
        let mut event = KestrelfsEvent::zeroed(abi::OP_LINK_DATA, req_id);
        event.payload[0..8].copy_from_slice(&parent_inode.to_le_bytes());
        event.payload[8..16].copy_from_slice(&inode_id.to_le_bytes());
        event.payload[16..18].copy_from_slice(&(name.len() as u16).to_le_bytes());
        event
    }

    fn decode_readdir_data_test_response(
        response: &KestrelfsEvent,
        data_buffer: &[u8; abi::DATA_BUFFER_SIZE],
    ) -> Vec<(u64, u8, String)> {
        assert_eq!(response.opcode, abi::OP_RESULT_OK);
        let count = u32::from_le_bytes(response.payload[0..4].try_into().unwrap()) as usize;
        let data_len =
            u32::from_le_bytes(response.payload[4..8].try_into().unwrap()) as usize;
        assert!(data_len <= data_buffer.len());

        let mut entries = Vec::with_capacity(count);
        let mut cursor = 0usize;
        for _ in 0..count {
            assert!(cursor + abi::READDIR_DATA_ENTRY_HEADER_SIZE <= data_len);
            let inode =
                u64::from_le_bytes(data_buffer[cursor..cursor + 8].try_into().unwrap());
            let name_len = u16::from_le_bytes(
                data_buffer[cursor + 8..cursor + 10].try_into().unwrap(),
            ) as usize;
            let dtype = data_buffer[cursor + 10];
            assert_eq!(data_buffer[cursor + 11], 0);
            cursor += abi::READDIR_DATA_ENTRY_HEADER_SIZE;
            assert!(name_len <= abi::NAME_DATA_MAX);
            assert!(cursor + name_len <= data_len);
            let name = std::str::from_utf8(&data_buffer[cursor..cursor + name_len])
                .unwrap()
                .to_string();
            cursor += name_len;
            entries.push((inode, dtype, name));
        }
        assert_eq!(cursor, data_len);
        entries
    }

    #[tokio::test]
    async fn link_data_supports_long_name_and_returns_updated_nlink() {
        let store: Arc<dyn MetaStore> = Arc::new(MemStore::new());
        let object_store: Arc<dyn ObjectStore> = Arc::new(object_store::MemObjectStore::new());
        let inode = store
            .create(fs_model::ROOT_INODE, "link-source", fs_model::S_IFREG | 0o644)
            .await
            .unwrap();
        let mut data = [0u8; abi::DATA_BUFFER_SIZE];
        let alias = "hard-link-name-longer-than-old-payload-limit";
        let request = raw_link_data_req(3200, fs_model::ROOT_INODE, inode, alias, &mut data);

        let response = build_response_with_data(
            &request,
            &store,
            &object_store,
            data.as_mut_ptr(),
        )
        .await;
        assert_eq!(response.opcode, abi::OP_RESULT_OK);
        assert_eq!(
            u32::from_le_bytes(response.payload[0..4].try_into().unwrap()),
            2
        );
        assert_eq!(store.lookup(fs_model::ROOT_INODE, alias).await.unwrap(), inode);
        assert_eq!(store.getattr(inode).await.unwrap().nlink, 2);
    }

    #[tokio::test]
    async fn name_data_end_to_end_long_names_survive_reload_and_unlink() {
        let temp_dir = TempDir::new().unwrap();
        let metadata_path = temp_dir.path().join("meta.json");
        let object_store: Arc<dyn ObjectStore> =
            Arc::new(object_store::MemObjectStore::new());
        let store: Arc<dyn MetaStore> =
            Arc::new(FileMetaStore::new(metadata_path.clone()).await.unwrap());
        let long_dir = format!("directory-{}", "d".repeat(48));
        let old_name = format!("created-file-{}", "o".repeat(48));
        let new_name = format!("renamed-file-{}", "n".repeat(48));
        assert!(long_dir.len() > 23 && old_name.len() > 23 && new_name.len() > 23);
        let mut data_buffer = [0u8; abi::DATA_BUFFER_SIZE];

        let mkdir = raw_name_data_req(
            abi::OP_MKDIR_DATA,
            800,
            fs_model::ROOT_INODE,
            0o1711,
            &long_dir,
            &mut data_buffer,
        );
        let mkdir_response = build_response_with_data(
            &mkdir,
            &store,
            &object_store,
            data_buffer.as_mut_ptr(),
        )
        .await;
        assert_eq!(mkdir_response.opcode, abi::OP_RESULT_OK);
        let dir_inode =
            u64::from_le_bytes(mkdir_response.payload[0..8].try_into().unwrap());
        let dir_attrs = store.getattr(dir_inode).await.unwrap();
        assert_eq!(dir_attrs.mode, fs_model::S_IFDIR | 0o1711);
        assert_eq!(dir_attrs.nlink, 2);
        assert_eq!(store.getattr(fs_model::ROOT_INODE).await.unwrap().nlink, 3);

        let create = raw_name_data_req(
            abi::OP_CREATE_DATA,
            801,
            dir_inode,
            fs_model::S_IFDIR | 0o2640,
            &old_name,
            &mut data_buffer,
        );
        let create_response = build_response_with_data(
            &create,
            &store,
            &object_store,
            data_buffer.as_mut_ptr(),
        )
        .await;
        assert_eq!(create_response.opcode, abi::OP_RESULT_OK);
        assert_eq!(
            u32::from_le_bytes(create_response.payload[16..20].try_into().unwrap()),
            fs_model::S_IFREG | 0o2640
        );
        assert_eq!(
            u32::from_le_bytes(create_response.payload[28..32].try_into().unwrap()),
            1
        );
        let file_inode =
            u64::from_le_bytes(create_response.payload[0..8].try_into().unwrap());

        let lookup = raw_name_data_req(
            abi::OP_LOOKUP_DATA,
            802,
            dir_inode,
            0,
            &old_name,
            &mut data_buffer,
        );
        let lookup_response = build_response_with_data(
            &lookup,
            &store,
            &object_store,
            data_buffer.as_mut_ptr(),
        )
        .await;
        assert_eq!(lookup_response.opcode, abi::OP_RESULT_OK);
        assert_eq!(
            u64::from_le_bytes(lookup_response.payload[0..8].try_into().unwrap()),
            file_inode
        );
        assert_eq!(
            u32::from_le_bytes(lookup_response.payload[16..20].try_into().unwrap()),
            fs_model::S_IFREG | 0o2640
        );

        let readdir = raw_readdir_data_req(803, dir_inode, 0);
        let readdir_response = build_response_with_data(
            &readdir,
            &store,
            &object_store,
            data_buffer.as_mut_ptr(),
        )
        .await;
        assert!(decode_readdir_data_test_response(&readdir_response, &data_buffer)
            .iter()
            .any(|(inode, dtype, name)| {
                *inode == file_inode && *dtype == libc::DT_REG && name == &old_name
            }));

        let rename = raw_rename_data_req(
            804,
            dir_inode,
            &old_name,
            dir_inode,
            &new_name,
            &mut data_buffer,
        );
        let rename_response = build_response_with_data(
            &rename,
            &store,
            &object_store,
            data_buffer.as_mut_ptr(),
        )
        .await;
        assert_eq!(rename_response.opcode, abi::OP_RESULT_OK);
        drop(store);

        let store: Arc<dyn MetaStore> =
            Arc::new(FileMetaStore::new(metadata_path).await.unwrap());
        let lookup_dir = raw_name_data_req(
            abi::OP_LOOKUP_DATA,
            805,
            fs_model::ROOT_INODE,
            0,
            &long_dir,
            &mut data_buffer,
        );
        let lookup_dir_response = build_response_with_data(
            &lookup_dir,
            &store,
            &object_store,
            data_buffer.as_mut_ptr(),
        )
        .await;
        assert_eq!(lookup_dir_response.opcode, abi::OP_RESULT_OK);
        assert_eq!(
            u32::from_le_bytes(lookup_dir_response.payload[16..20].try_into().unwrap()),
            fs_model::S_IFDIR | 0o1711
        );
        assert_eq!(
            u32::from_le_bytes(lookup_dir_response.payload[28..32].try_into().unwrap()),
            2
        );

        let lookup_new = raw_name_data_req(
            abi::OP_LOOKUP_DATA,
            806,
            dir_inode,
            0,
            &new_name,
            &mut data_buffer,
        );
        let lookup_new_response = build_response_with_data(
            &lookup_new,
            &store,
            &object_store,
            data_buffer.as_mut_ptr(),
        )
        .await;
        assert_eq!(lookup_new_response.opcode, abi::OP_RESULT_OK);
        assert_eq!(
            u32::from_le_bytes(lookup_new_response.payload[16..20].try_into().unwrap()),
            fs_model::S_IFREG | 0o2640
        );

        let readdir = raw_readdir_data_req(807, dir_inode, 0);
        let readdir_response = build_response_with_data(
            &readdir,
            &store,
            &object_store,
            data_buffer.as_mut_ptr(),
        )
        .await;
        assert!(decode_readdir_data_test_response(&readdir_response, &data_buffer)
            .iter()
            .any(|(inode, dtype, name)| {
                *inode == file_inode && *dtype == libc::DT_REG && name == &new_name
            }));

        let unlink = raw_name_data_req(
            abi::OP_UNLINK_DATA,
            808,
            dir_inode,
            0,
            &new_name,
            &mut data_buffer,
        );
        let unlink_response = build_response_with_data(
            &unlink,
            &store,
            &object_store,
            data_buffer.as_mut_ptr(),
        )
        .await;
        assert_eq!(unlink_response.opcode, abi::OP_RESULT_OK);
        assert!(matches!(
            store.lookup(dir_inode, &new_name).await,
            Err(MetaError::NotFound)
        ));
    }

    #[tokio::test]
    async fn readdir_data_batches_multiple_long_entries() {
        let store: Arc<dyn MetaStore> = Arc::new(MemStore::new());
        let object_store: Arc<dyn ObjectStore> =
            Arc::new(object_store::MemObjectStore::new());
        let mut expected = Vec::new();
        for index in 0..20 {
            let name = format!("batch-entry-{index:02}-{}", "x".repeat(40));
            let inode = store
                .create(fs_model::ROOT_INODE, &name, fs_model::S_IFREG | 0o644)
                .await
                .unwrap();
            expected.push((inode, name));
        }

        let mut data_buffer = [0u8; abi::DATA_BUFFER_SIZE];
        let request = raw_readdir_data_req(809, fs_model::ROOT_INODE, 0);
        let response = build_response_with_data(
            &request,
            &store,
            &object_store,
            data_buffer.as_mut_ptr(),
        )
        .await;
        let entries = decode_readdir_data_test_response(&response, &data_buffer);
        for expected_entry in expected {
            assert!(entries.iter().any(|(inode, dtype, name)| {
                *inode == expected_entry.0
                    && *dtype == libc::DT_REG
                    && name == &expected_entry.1
            }));
        }
    }

    #[tokio::test]
    async fn readdir_data_reports_file_dir_symlink_and_whiteout_types() {
        let store: Arc<dyn MetaStore> = Arc::new(MemStore::new());
        let object_store: Arc<dyn ObjectStore> =
            Arc::new(object_store::MemObjectStore::new());
        store
            .create(fs_model::ROOT_INODE, "dtype-file", fs_model::S_IFREG | 0o644)
            .await
            .unwrap();
        store
            .mkdir(fs_model::ROOT_INODE, "dtype-dir", fs_model::S_IFDIR | 0o755)
            .await
            .unwrap();
        store
            .symlink(fs_model::ROOT_INODE, "dtype-link", "dtype-file")
            .await
            .unwrap();
        store
            .create(fs_model::ROOT_INODE, "dtype-whiteout", fs_model::S_IFCHR)
            .await
            .unwrap();

        let mut data_buffer = [0u8; abi::DATA_BUFFER_SIZE];
        let request = raw_readdir_data_req(810, fs_model::ROOT_INODE, 0);
        let response = build_response_with_data(
            &request,
            &store,
            &object_store,
            data_buffer.as_mut_ptr(),
        )
        .await;
        let entries = decode_readdir_data_test_response(&response, &data_buffer);
        for (name, expected_type) in [
            ("dtype-file", libc::DT_REG),
            ("dtype-dir", libc::DT_DIR),
            ("dtype-link", libc::DT_LNK),
            ("dtype-whiteout", libc::DT_CHR),
        ] {
            assert!(entries
                .iter()
                .any(|(_, dtype, entry_name)| *dtype == expected_type && entry_name == name));
        }
    }

    #[tokio::test]
    async fn name_data_accepts_name_max_boundary() {
        let store: Arc<dyn MetaStore> = Arc::new(MemStore::new());
        let object_store: Arc<dyn ObjectStore> =
            Arc::new(object_store::MemObjectStore::new());
        let max_name = "m".repeat(abi::NAME_DATA_MAX);
        let mut data_buffer = [0u8; abi::DATA_BUFFER_SIZE];

        let create = raw_name_data_req(
            abi::OP_CREATE_DATA,
            810,
            fs_model::ROOT_INODE,
            fs_model::S_IFREG | 0o644,
            &max_name,
            &mut data_buffer,
        );
        let create_response = build_response_with_data(
            &create,
            &store,
            &object_store,
            data_buffer.as_mut_ptr(),
        )
        .await;
        assert_eq!(create_response.opcode, abi::OP_RESULT_OK);
        let inode = u64::from_le_bytes(create_response.payload[0..8].try_into().unwrap());

        let lookup = raw_name_data_req(
            abi::OP_LOOKUP_DATA,
            811,
            fs_model::ROOT_INODE,
            0,
            &max_name,
            &mut data_buffer,
        );
        let lookup_response = build_response_with_data(
            &lookup,
            &store,
            &object_store,
            data_buffer.as_mut_ptr(),
        )
        .await;
        assert_eq!(lookup_response.opcode, abi::OP_RESULT_OK);

        let readdir = raw_readdir_data_req(812, fs_model::ROOT_INODE, 0);
        let readdir_response = build_response_with_data(
            &readdir,
            &store,
            &object_store,
            data_buffer.as_mut_ptr(),
        )
        .await;
        assert!(decode_readdir_data_test_response(&readdir_response, &data_buffer)
            .contains(&(inode, libc::DT_REG, max_name.clone())));

        let unlink = raw_name_data_req(
            abi::OP_UNLINK_DATA,
            813,
            fs_model::ROOT_INODE,
            0,
            &max_name,
            &mut data_buffer,
        );
        let unlink_response = build_response_with_data(
            &unlink,
            &store,
            &object_store,
            data_buffer.as_mut_ptr(),
        )
        .await;
        assert_eq!(unlink_response.opcode, abi::OP_RESULT_OK);
    }

    #[tokio::test]
    async fn rename_data_long_name_same_directory() {
        let store: Arc<dyn MetaStore> = Arc::new(MemStore::new());
        let object_store: Arc<dyn ObjectStore> =
            Arc::new(object_store::MemObjectStore::new());
        let source_inode = store
            .create(fs_model::ROOT_INODE, "source", fs_model::S_IFREG | 0o644)
            .await
            .unwrap();
        let long_name = "same-directory-long.txt";
        assert!(long_name.len() >= 20);

        let mut data_buffer = [0u8; abi::DATA_BUFFER_SIZE];
        let request = raw_rename_data_req(
            700,
            fs_model::ROOT_INODE,
            "source",
            fs_model::ROOT_INODE,
            long_name,
            &mut data_buffer,
        );
        let response = build_response_with_data(
            &request,
            &store,
            &object_store,
            data_buffer.as_mut_ptr(),
        )
        .await;

        assert_eq!(response.opcode, abi::OP_RESULT_OK);
        assert_eq!(store.lookup(fs_model::ROOT_INODE, long_name).await.unwrap(), source_inode);
        assert!(matches!(
            store.lookup(fs_model::ROOT_INODE, "source").await,
            Err(MetaError::NotFound)
        ));
    }

    #[tokio::test]
    async fn rename_data_noreplace_returns_eexist_without_mutation() {
        let store: Arc<dyn MetaStore> = Arc::new(MemStore::new());
        let object_store: Arc<dyn ObjectStore> =
            Arc::new(object_store::MemObjectStore::new());
        let source = store
            .create(fs_model::ROOT_INODE, "source", fs_model::S_IFREG | 0o644)
            .await
            .unwrap();
        let target = store
            .create(fs_model::ROOT_INODE, "target", fs_model::S_IFREG | 0o644)
            .await
            .unwrap();
        let mut data_buffer = [0u8; abi::DATA_BUFFER_SIZE];
        let request = raw_rename_data_req_with_flags(
            705,
            fs_model::ROOT_INODE,
            "source",
            fs_model::ROOT_INODE,
            "target",
            abi::RENAME_NOREPLACE,
            &mut data_buffer,
        );

        let response = build_response_with_data(
            &request,
            &store,
            &object_store,
            data_buffer.as_mut_ptr(),
        )
        .await;
        assert_eq!(response.opcode, abi::OP_RESULT_ERROR);
        assert_eq!(response.error_code, -libc::EEXIST);
        assert_eq!(store.lookup(fs_model::ROOT_INODE, "source").await.unwrap(), source);
        assert_eq!(store.lookup(fs_model::ROOT_INODE, "target").await.unwrap(), target);
        assert!(store.pending_garbage().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn rename_data_exchange_swaps_existing_paths_without_gc() {
        let store: Arc<dyn MetaStore> = Arc::new(MemStore::new());
        let object_store: Arc<dyn ObjectStore> =
            Arc::new(object_store::MemObjectStore::new());
        let left = store
            .create(fs_model::ROOT_INODE, "exchange-left", fs_model::S_IFREG | 0o640)
            .await
            .unwrap();
        let right = store
            .create(fs_model::ROOT_INODE, "exchange-right", fs_model::S_IFREG | 0o600)
            .await
            .unwrap();
        let mut data_buffer = [0u8; abi::DATA_BUFFER_SIZE];
        let request = raw_rename_data_req_with_flags(
            707,
            fs_model::ROOT_INODE,
            "exchange-left",
            fs_model::ROOT_INODE,
            "exchange-right",
            abi::RENAME_EXCHANGE,
            &mut data_buffer,
        );

        let response = build_response_with_data(
            &request,
            &store,
            &object_store,
            data_buffer.as_mut_ptr(),
        )
        .await;
        assert_eq!(response.opcode, abi::OP_RESULT_OK);
        assert_eq!(store.lookup(fs_model::ROOT_INODE, "exchange-left").await.unwrap(), right);
        assert_eq!(store.lookup(fs_model::ROOT_INODE, "exchange-right").await.unwrap(), left);
        assert!(store.pending_garbage().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn rename_data_whiteout_moves_source_and_exposes_char_marker() {
        let store: Arc<dyn MetaStore> = Arc::new(MemStore::new());
        let object_store: Arc<dyn ObjectStore> =
            Arc::new(object_store::MemObjectStore::new());
        let source = store
            .create(fs_model::ROOT_INODE, "whiteout-source", fs_model::S_IFREG | 0o640)
            .await
            .unwrap();
        let mut data_buffer = [0u8; abi::DATA_BUFFER_SIZE];
        let request = raw_rename_data_req_with_flags(
            708,
            fs_model::ROOT_INODE,
            "whiteout-source",
            fs_model::ROOT_INODE,
            "whiteout-target",
            abi::RENAME_WHITEOUT,
            &mut data_buffer,
        );

        let response = build_response_with_data(
            &request,
            &store,
            &object_store,
            data_buffer.as_mut_ptr(),
        )
        .await;
        assert_eq!(response.opcode, abi::OP_RESULT_OK);
        assert_eq!(
            store.lookup(fs_model::ROOT_INODE, "whiteout-target").await.unwrap(),
            source
        );
        let marker = store
            .lookup(fs_model::ROOT_INODE, "whiteout-source")
            .await
            .unwrap();
        assert_ne!(marker, source);
        assert_eq!(store.getattr(marker).await.unwrap().mode, fs_model::S_IFCHR);
    }

    #[tokio::test]
    async fn rename_data_noreplace_same_inode_aliases_is_successful_noop() {
        let store: Arc<dyn MetaStore> = Arc::new(MemStore::new());
        let object_store: Arc<dyn ObjectStore> =
            Arc::new(object_store::MemObjectStore::new());
        let inode = store
            .create(fs_model::ROOT_INODE, "source", fs_model::S_IFREG | 0o644)
            .await
            .unwrap();
        store.link(fs_model::ROOT_INODE, "alias", inode).await.unwrap();
        let mut data_buffer = [0u8; abi::DATA_BUFFER_SIZE];
        let request = raw_rename_data_req_with_flags(
            706,
            fs_model::ROOT_INODE,
            "source",
            fs_model::ROOT_INODE,
            "alias",
            abi::RENAME_NOREPLACE,
            &mut data_buffer,
        );

        let response = build_response_with_data(
            &request,
            &store,
            &object_store,
            data_buffer.as_mut_ptr(),
        )
        .await;
        assert_eq!(response.opcode, abi::OP_RESULT_OK);
        assert_eq!(store.lookup(fs_model::ROOT_INODE, "source").await.unwrap(), inode);
        assert_eq!(store.lookup(fs_model::ROOT_INODE, "alias").await.unwrap(), inode);
        assert_eq!(store.getattr(inode).await.unwrap().nlink, 2);
    }

    #[tokio::test]
    async fn rename_data_long_name_cross_directory() {
        let store: Arc<dyn MetaStore> = Arc::new(MemStore::new());
        let object_store: Arc<dyn ObjectStore> =
            Arc::new(object_store::MemObjectStore::new());
        let old_parent = store
            .mkdir(fs_model::ROOT_INODE, "old-dir", 0o755)
            .await
            .unwrap();
        let new_parent = store
            .mkdir(fs_model::ROOT_INODE, "new-dir", 0o755)
            .await
            .unwrap();
        let source_inode = store
            .create(old_parent, "source", fs_model::S_IFREG | 0o644)
            .await
            .unwrap();
        let long_name = "cross-directory-moved";

        let mut data_buffer = [0u8; abi::DATA_BUFFER_SIZE];
        let request = raw_rename_data_req(
            701,
            old_parent,
            "source",
            new_parent,
            long_name,
            &mut data_buffer,
        );
        let response = build_response_with_data(
            &request,
            &store,
            &object_store,
            data_buffer.as_mut_ptr(),
        )
        .await;

        assert_eq!(response.opcode, abi::OP_RESULT_OK);
        assert_eq!(store.lookup(new_parent, long_name).await.unwrap(), source_inode);
        assert!(matches!(
            store.lookup(old_parent, "source").await,
            Err(MetaError::NotFound)
        ));
    }

    #[tokio::test]
    async fn rename_data_accepts_short_and_name_max_names() {
        let store: Arc<dyn MetaStore> = Arc::new(MemStore::new());
        let object_store: Arc<dyn ObjectStore> =
            Arc::new(object_store::MemObjectStore::new());
        let source_inode = store
            .create(fs_model::ROOT_INODE, "a", fs_model::S_IFREG | 0o644)
            .await
            .unwrap();
        let mut data_buffer = [0u8; abi::DATA_BUFFER_SIZE];

        let short_request = raw_rename_data_req(
            702,
            fs_model::ROOT_INODE,
            "a",
            fs_model::ROOT_INODE,
            "b",
            &mut data_buffer,
        );
        let short_response = build_response_with_data(
            &short_request,
            &store,
            &object_store,
            data_buffer.as_mut_ptr(),
        )
        .await;
        assert_eq!(short_response.opcode, abi::OP_RESULT_OK);

        let max_name = "x".repeat(abi::RENAME_DATA_NAME_MAX);
        let max_request = raw_rename_data_req(
            703,
            fs_model::ROOT_INODE,
            "b",
            fs_model::ROOT_INODE,
            &max_name,
            &mut data_buffer,
        );
        let max_response = build_response_with_data(
            &max_request,
            &store,
            &object_store,
            data_buffer.as_mut_ptr(),
        )
        .await;

        assert_eq!(max_response.opcode, abi::OP_RESULT_OK);
        assert_eq!(store.lookup(fs_model::ROOT_INODE, &max_name).await.unwrap(), source_inode);
    }

    #[tokio::test]
    async fn rename_data_long_name_survives_file_meta_store_reload() {
        let temp_dir = TempDir::new().unwrap();
        let metadata_path = temp_dir.path().join("meta.json");
        let store: Arc<dyn MetaStore> =
            Arc::new(FileMetaStore::new(metadata_path.clone()).await.unwrap());
        let object_store: Arc<dyn ObjectStore> =
            Arc::new(object_store::MemObjectStore::new());
        let source_inode = store
            .create(fs_model::ROOT_INODE, "before", fs_model::S_IFREG | 0o644)
            .await
            .unwrap();
        let long_name = "persisted-rename-long";
        let mut data_buffer = [0u8; abi::DATA_BUFFER_SIZE];
        let request = raw_rename_data_req(
            704,
            fs_model::ROOT_INODE,
            "before",
            fs_model::ROOT_INODE,
            long_name,
            &mut data_buffer,
        );

        let response =
            handle_rename_data(&request, &store, &object_store, data_buffer.as_ptr()).await;
        assert_eq!(response.opcode, abi::OP_RESULT_OK);
        drop(store);

        let reloaded = FileMetaStore::new(metadata_path).await.unwrap();
        assert_eq!(reloaded.lookup(fs_model::ROOT_INODE, long_name).await.unwrap(), source_inode);
        assert!(matches!(
            reloaded.lookup(fs_model::ROOT_INODE, "before").await,
            Err(MetaError::NotFound)
        ));
    }

    #[test]
    fn rename_same_directory() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let store = Arc::new(meta::MemStore::new()) as Arc<dyn MetaStore>;
        let object_store = Arc::new(object_store::MemObjectStore::new()) as Arc<dyn ObjectStore>;

        // 1. Create a file
        let create_req = raw_create_req(300, fs_model::ROOT_INODE, fs_model::S_IFREG | 0o644, "old.txt");
        let create_resp = runtime.block_on(build_response(&create_req, &store, &object_store));
        assert_eq!(create_resp.opcode, abi::OP_RESULT_OK);
        let file_ino = u64::from_le_bytes(create_resp.payload[0..8].try_into().unwrap());

        // 2. Rename in same directory
        let rename_req = raw_rename_req(301, fs_model::ROOT_INODE, "old.txt", fs_model::ROOT_INODE, "new.txt");
        let rename_resp = runtime.block_on(build_response(&rename_req, &store, &object_store));
        assert_eq!(rename_resp.opcode, abi::OP_RESULT_OK);

        // 3. Old name should not exist
        let lookup_old = raw_lookup_req(302, fs_model::ROOT_INODE, "old.txt");
        let lookup_old_resp = runtime.block_on(build_response(&lookup_old, &store, &object_store));
        assert_eq!(lookup_old_resp.opcode, abi::OP_RESULT_ERROR);
        assert_eq!(lookup_old_resp.error_code, -libc::ENOENT);

        // 4. New name should exist with same inode
        let lookup_new = raw_lookup_req(303, fs_model::ROOT_INODE, "new.txt");
        let lookup_new_resp = runtime.block_on(build_response(&lookup_new, &store, &object_store));
        assert_eq!(lookup_new_resp.opcode, abi::OP_RESULT_OK);
        let found_ino = u64::from_le_bytes(lookup_new_resp.payload[0..8].try_into().unwrap());
        assert_eq!(found_ino, file_ino);
    }

    #[test]
    fn rename_cross_directory() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let store = Arc::new(meta::MemStore::new()) as Arc<dyn MetaStore>;
        let object_store = Arc::new(object_store::MemObjectStore::new()) as Arc<dyn ObjectStore>;

        // 1. Create two directories
        let mkdir1_req = raw_mkdir_req(400, fs_model::ROOT_INODE, 0o755, "dir1");
        let mkdir1_resp = runtime.block_on(build_response(&mkdir1_req, &store, &object_store));
        assert_eq!(mkdir1_resp.opcode, abi::OP_RESULT_OK);
        let dir1_ino = u64::from_le_bytes(mkdir1_resp.payload[0..8].try_into().unwrap());

        let mkdir2_req = raw_mkdir_req(401, fs_model::ROOT_INODE, 0o755, "dir2");
        let mkdir2_resp = runtime.block_on(build_response(&mkdir2_req, &store, &object_store));
        assert_eq!(mkdir2_resp.opcode, abi::OP_RESULT_OK);
        let dir2_ino = u64::from_le_bytes(mkdir2_resp.payload[0..8].try_into().unwrap());

        // 2. Create file in dir1
        let create_req = raw_create_req(402, dir1_ino, fs_model::S_IFREG | 0o644, "file");
        let create_resp = runtime.block_on(build_response(&create_req, &store, &object_store));
        assert_eq!(create_resp.opcode, abi::OP_RESULT_OK);
        let file_ino = u64::from_le_bytes(create_resp.payload[0..8].try_into().unwrap());

        // 3. Move from dir1 to dir2
        let rename_req = raw_rename_req(403, dir1_ino, "file", dir2_ino, "moved");
        let rename_resp = runtime.block_on(build_response(&rename_req, &store, &object_store));
        assert_eq!(rename_resp.opcode, abi::OP_RESULT_OK);

        // 4. Should not exist in dir1
        let lookup_old = raw_lookup_req(404, dir1_ino, "file");
        let lookup_old_resp = runtime.block_on(build_response(&lookup_old, &store, &object_store));
        assert_eq!(lookup_old_resp.opcode, abi::OP_RESULT_ERROR);

        // 5. Should exist in dir2
        let lookup_new = raw_lookup_req(405, dir2_ino, "moved");
        let lookup_new_resp = runtime.block_on(build_response(&lookup_new, &store, &object_store));
        assert_eq!(lookup_new_resp.opcode, abi::OP_RESULT_OK);
        let found_ino = u64::from_le_bytes(lookup_new_resp.payload[0..8].try_into().unwrap());
        assert_eq!(found_ino, file_ino);
    }

    #[test]
    fn rename_replaces_existing_file() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let store = Arc::new(meta::MemStore::new()) as Arc<dyn MetaStore>;
        let object_store = Arc::new(object_store::MemObjectStore::new()) as Arc<dyn ObjectStore>;

        // 1. Create two files
        let create1_req = raw_create_req(500, fs_model::ROOT_INODE, fs_model::S_IFREG | 0o644, "file1");
        let create1_resp = runtime.block_on(build_response(&create1_req, &store, &object_store));
        assert_eq!(create1_resp.opcode, abi::OP_RESULT_OK);
        let file1_ino = u64::from_le_bytes(create1_resp.payload[0..8].try_into().unwrap());

        let create2_req = raw_create_req(501, fs_model::ROOT_INODE, fs_model::S_IFREG | 0o644, "file2");
        let create2_resp = runtime.block_on(build_response(&create2_req, &store, &object_store));
        assert_eq!(create2_resp.opcode, abi::OP_RESULT_OK);

        // 2. Rename file1 over file2 (atomic replacement)
        let rename_req = raw_rename_req(502, fs_model::ROOT_INODE, "file1", fs_model::ROOT_INODE, "file2");
        let rename_resp = runtime.block_on(build_response(&rename_req, &store, &object_store));
        assert_eq!(rename_resp.opcode, abi::OP_RESULT_OK);

        // 3. file2 should now point to file1's inode
        let lookup_req = raw_lookup_req(503, fs_model::ROOT_INODE, "file2");
        let lookup_resp = runtime.block_on(build_response(&lookup_req, &store, &object_store));
        assert_eq!(lookup_resp.opcode, abi::OP_RESULT_OK);
        let found_ino = u64::from_le_bytes(lookup_resp.payload[0..8].try_into().unwrap());
        assert_eq!(found_ino, file1_ino);
    }

    #[test]
    fn rename_rejects_nonempty_directory() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let store = Arc::new(meta::MemStore::new()) as Arc<dyn MetaStore>;
        let object_store = Arc::new(object_store::MemObjectStore::new()) as Arc<dyn ObjectStore>;

        // 1. Create directory with a file inside
        let mkdir_req = raw_mkdir_req(600, fs_model::ROOT_INODE, 0o755, "dir");
        let mkdir_resp = runtime.block_on(build_response(&mkdir_req, &store, &object_store));
        let dir_ino = u64::from_le_bytes(mkdir_resp.payload[0..8].try_into().unwrap());

        let create_req = raw_create_req(601, dir_ino, fs_model::S_IFREG | 0o644, "child");
        runtime.block_on(build_response(&create_req, &store, &object_store));

        // 2. Create another file
        let create_file_req = raw_create_req(602, fs_model::ROOT_INODE, fs_model::S_IFREG | 0o644, "file");
        runtime.block_on(build_response(&create_file_req, &store, &object_store));

        // 3. Try to rename file over non-empty directory
        let rename_req = raw_rename_req(603, fs_model::ROOT_INODE, "file", fs_model::ROOT_INODE, "dir");
        let rename_resp = runtime.block_on(build_response(&rename_req, &store, &object_store));
        assert_eq!(rename_resp.opcode, abi::OP_RESULT_ERROR);
        assert_eq!(rename_resp.error_code, -libc::ENOTEMPTY);
    }

    #[tokio::test]
    async fn symlink_data_end_to_end_survives_rename_and_reload() {
        fn raw_symlink_data_req(
            req_id: u64,
            parent: u64,
            name: &str,
            target: &str,
            data_buffer: &mut [u8; abi::DATA_BUFFER_SIZE],
        ) -> KestrelfsEvent {
            let name = name.as_bytes();
            let target = target.as_bytes();
            data_buffer[..name.len()].copy_from_slice(name);
            data_buffer[name.len()..name.len() + target.len()].copy_from_slice(target);
            let mut request = KestrelfsEvent::zeroed(abi::OP_SYMLINK_DATA, req_id);
            request.payload[0..8].copy_from_slice(&parent.to_le_bytes());
            request.payload[8..10].copy_from_slice(&(name.len() as u16).to_le_bytes());
            request.payload[10..12].copy_from_slice(&(target.len() as u16).to_le_bytes());
            request
        }

        let temp_dir = TempDir::new().unwrap();
        let metadata_path = temp_dir.path().join("meta.json");
        let object_store: Arc<dyn ObjectStore> =
            Arc::new(object_store::MemObjectStore::new());
        let store: Arc<dyn MetaStore> =
            Arc::new(FileMetaStore::new(metadata_path.clone()).await.unwrap());
        let mut data_buffer = [0u8; abi::DATA_BUFFER_SIZE];
        let name = "long-symbolic-link-name-for-step-14";
        let renamed = "renamed-symbolic-link-name-for-step-14";
        let target = "../some/relative/target-with-a-long-name.txt";

        let request = raw_symlink_data_req(
            900,
            fs_model::ROOT_INODE,
            name,
            target,
            &mut data_buffer,
        );
        let response = build_response_with_data(
            &request,
            &store,
            &object_store,
            data_buffer.as_mut_ptr(),
        )
        .await;
        assert_eq!(response.opcode, abi::OP_RESULT_OK);
        let inode = u64::from_le_bytes(response.payload[0..8].try_into().unwrap());
        assert_eq!(
            u32::from_le_bytes(response.payload[16..20].try_into().unwrap()) & 0o170000,
            fs_model::S_IFLNK
        );

        let mut readlink = KestrelfsEvent::zeroed(abi::OP_READLINK_DATA, 901);
        readlink.payload[0..8].copy_from_slice(&inode.to_le_bytes());
        let response = build_response_with_data(
            &readlink,
            &store,
            &object_store,
            data_buffer.as_mut_ptr(),
        )
        .await;
        let target_len =
            u32::from_le_bytes(response.payload[0..4].try_into().unwrap()) as usize;
        assert_eq!(&data_buffer[..target_len], target.as_bytes());

        let rename = raw_rename_data_req(
            902,
            fs_model::ROOT_INODE,
            name,
            fs_model::ROOT_INODE,
            renamed,
            &mut data_buffer,
        );
        let response = build_response_with_data(
            &rename,
            &store,
            &object_store,
            data_buffer.as_mut_ptr(),
        )
        .await;
        assert_eq!(response.opcode, abi::OP_RESULT_OK);
        drop(store);

        let restored: Arc<dyn MetaStore> =
            Arc::new(FileMetaStore::new(metadata_path).await.unwrap());
        assert_eq!(
            restored.lookup(fs_model::ROOT_INODE, renamed).await.unwrap(),
            inode
        );
        assert_eq!(restored.readlink(inode).await.unwrap(), target);
    }

    #[tokio::test]
    async fn localfs_write_then_unlink_deletes_object_file() {
        let temp_dir = TempDir::new().unwrap();
        let local_store = LocalFsObjectStore::new(temp_dir.path()).await.unwrap();
        let object_store: Arc<dyn ObjectStore> = Arc::new(local_store.clone());
        let store: Arc<dyn MetaStore> = Arc::new(MemStore::new());
        let inode = store
            .create(fs_model::ROOT_INODE, "gc-unlink", fs_model::S_IFREG | 0o644)
            .await
            .unwrap();

        let response = handle_write_bytes(
            1000,
            inode,
            0,
            b"garbage collected payload".to_vec(),
            "TEST_WRITE",
            &store,
            &object_store,
        )
        .await;
        assert_eq!(response.opcode, abi::OP_RESULT_OK);
        let slice = store.read_slices(inode, 0).await.unwrap().pop().unwrap();
        let key = slice.block_key(0);
        assert!(temp_dir.path().join(&key).is_file());

        let unlink = raw_unlink_req(1001, fs_model::ROOT_INODE, "gc-unlink");
        let response = build_response(&unlink, &store, &object_store).await;
        assert_eq!(response.opcode, abi::OP_RESULT_OK);
        assert!(matches!(
            local_store.get(&key).await,
            Err(object_store::ObjectStoreError::NotFound(_))
        ));
        assert!(!temp_dir.path().join(&key).exists());
    }

    #[tokio::test]
    async fn mem_gc_delete_failure_is_queued_and_retried_idempotently() {
        let flaky = FailOnceObjectStore::new();
        let object_store: Arc<dyn ObjectStore> = Arc::new(flaky.clone());
        let store: Arc<dyn MetaStore> = Arc::new(MemStore::new());
        let inode = store
            .create(fs_model::ROOT_INODE, "gc-retry", fs_model::S_IFREG | 0o644)
            .await
            .unwrap();
        let slice = Slice {
            chunk_index: 0,
            slice_id: uuid::Uuid::new_v4(),
            chunk_offset: 0,
            length: 32,
            written_at: meta::current_unix_time(),
        };
        let key = slice.block_key(0);
        flaky.put(key.clone(), vec![42; 32]).await.unwrap();
        store.append_slice(inode, slice).await.unwrap();
        let garbage = store
            .unlink(fs_model::ROOT_INODE, "gc-retry")
            .await
            .unwrap();

        let first = delete_garbage_objects(
            "TEST_FAIL_ONCE",
            Some(&garbage),
            &store,
            &object_store,
        )
        .await;
        assert!(first.retry_needed);
        assert!(flaky.get(&key).await.is_ok());
        assert_eq!(store.pending_garbage().await.unwrap(), vec![key.clone()]);

        let second = delete_garbage_objects("TEST_RETRY", None, &store, &object_store).await;
        assert!(!second.retry_needed);
        assert!(matches!(flaky.get(&key).await, Err(ObjectStoreError::NotFound(_))));
        assert!(store.pending_garbage().await.unwrap().is_empty());

        // A post-ack pass is a no-op, proving repeated recovery is safe.
        let third = delete_garbage_objects("TEST_IDEMPOTENT", None, &store, &object_store).await;
        assert!(!third.retry_needed);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn async_gc_failure_stays_durable_until_worker_retry_and_ack() {
        let flaky = FailOnceObjectStore::new();
        let object_store: Arc<dyn ObjectStore> = Arc::new(flaky.clone());
        let store: Arc<dyn MetaStore> = Arc::new(MemStore::new());
        let inode = store
            .create(
                fs_model::ROOT_INODE,
                "gc-worker-retry",
                fs_model::S_IFREG | 0o644,
            )
            .await
            .unwrap();
        let slice = Slice {
            chunk_index: 0,
            slice_id: uuid::Uuid::new_v4(),
            chunk_offset: 0,
            length: 32,
            written_at: meta::current_unix_time(),
        };
        let key = slice.block_key(0);
        flaky.put(key.clone(), vec![42; 32]).await.unwrap();
        store.append_slice(inode, slice).await.unwrap();
        let garbage = store
            .unlink(fs_model::ROOT_INODE, "gc-worker-retry")
            .await
            .unwrap();

        let mut worker = GcWorker::start(&tokio::runtime::Handle::current(), object_store);
        let scheduler = worker.scheduler();
        assert_eq!(scheduler.schedule("TEST_ASYNC_FAIL", garbage).queued, 1);
        let first = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(completion) = worker.try_recv() {
                    break completion;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(apply_gc_completion(first, &store, &scheduler).await);
        assert_eq!(store.pending_garbage().await.unwrap(), vec![key.clone()]);
        assert!(flaky.get(&key).await.is_ok());

        assert_eq!(scheduler.schedule("TEST_ASYNC_RETRY", vec![key.clone()]).queued, 1);
        let second = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(completion) = worker.try_recv() {
                    break completion;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!apply_gc_completion(second, &store, &scheduler).await);
        assert!(store.pending_garbage().await.unwrap().is_empty());
        assert!(matches!(flaky.get(&key).await, Err(ObjectStoreError::NotFound(_))));
    }

    #[tokio::test]
    async fn file_meta_restart_replays_gc_queue_into_localfs() {
        let temp_dir = TempDir::new().unwrap();
        let metadata_path = temp_dir.path().join("meta.json");
        let local_store = LocalFsObjectStore::new(temp_dir.path()).await.unwrap();
        let object_store: Arc<dyn ObjectStore> = Arc::new(local_store.clone());
        let key;

        {
            let store: Arc<dyn MetaStore> =
                Arc::new(FileMetaStore::new(metadata_path.clone()).await.unwrap());
            let inode = store
                .create(
                    fs_model::ROOT_INODE,
                    "crash-before-delete",
                    fs_model::S_IFREG | 0o644,
                )
                .await
                .unwrap();
            let slice = Slice {
                chunk_index: 0,
                slice_id: uuid::Uuid::new_v4(),
                chunk_offset: 0,
                length: 48,
                written_at: meta::current_unix_time(),
            };
            key = slice.block_key(0);
            local_store.put(key.clone(), vec![7; 48]).await.unwrap();
            store.append_slice(inode, slice).await.unwrap();
            store
                .unlink(fs_model::ROOT_INODE, "crash-before-delete")
                .await
                .unwrap();
            assert_eq!(store.pending_garbage().await.unwrap(), vec![key.clone()]);
            // Drop without calling delete_garbage_objects: this models a crash
            // immediately after the atomic metadata+queue commit.
        }

        let restarted: Arc<dyn MetaStore> =
            Arc::new(FileMetaStore::new(metadata_path.clone()).await.unwrap());
        assert_eq!(restarted.pending_garbage().await.unwrap(), vec![key.clone()]);
        assert!(local_store.get(&key).await.is_ok());
        let outcome =
            delete_garbage_objects("TEST_STARTUP", None, &restarted, &object_store).await;
        assert!(!outcome.retry_needed);
        assert!(matches!(
            local_store.get(&key).await,
            Err(ObjectStoreError::NotFound(_))
        ));
        drop(restarted);

        let restarted = FileMetaStore::new(metadata_path).await.unwrap();
        assert!(restarted.pending_garbage().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn s3_environment_gated_unlink_gc_deletes_object() {
        let (Ok(endpoint), Ok(bucket), Ok(_access_key), Ok(_secret_key)) = (
            std::env::var("S3_ENDPOINT"),
            std::env::var("S3_BUCKET"),
            std::env::var("AWS_ACCESS_KEY_ID"),
            std::env::var("AWS_SECRET_ACCESS_KEY"),
        ) else {
            eprintln!("S3 integration environment not set; skipping S3 GC assertions");
            return;
        };
        let prefix = format!("kestrelfs-gc-tests/{}", uuid::Uuid::new_v4());
        let s3 = S3ObjectStore::new(&format!("s3://{bucket}/{prefix}"), Some(&endpoint))
            .await
            .unwrap();
        s3.ensure_bucket_for_test().await;
        let object_store: Arc<dyn ObjectStore> = Arc::new(s3.clone());
        let store: Arc<dyn MetaStore> = Arc::new(MemStore::new());
        let inode = store
            .create(fs_model::ROOT_INODE, "s3-gc", fs_model::S_IFREG | 0o644)
            .await
            .unwrap();

        let response = handle_write_bytes(
            1002,
            inode,
            0,
            b"S3 garbage collected payload".to_vec(),
            "TEST_S3_WRITE",
            &store,
            &object_store,
        )
        .await;
        assert_eq!(response.opcode, abi::OP_RESULT_OK);
        let key = store
            .read_slices(inode, 0)
            .await
            .unwrap()
            .pop()
            .unwrap()
            .block_key(0);
        assert_eq!(s3.get(&key).await.unwrap(), b"S3 garbage collected payload");

        let unlink = raw_unlink_req(1003, fs_model::ROOT_INODE, "s3-gc");
        let response = build_response(&unlink, &store, &object_store).await;
        assert_eq!(response.opcode, abi::OP_RESULT_OK);
        assert!(matches!(
            s3.get(&key).await,
            Err(object_store::ObjectStoreError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn rename_overwrite_deletes_replaced_files_objects() {
        let mem_objects = object_store::MemObjectStore::new();
        let object_store: Arc<dyn ObjectStore> = Arc::new(mem_objects.clone());
        let store: Arc<dyn MetaStore> = Arc::new(MemStore::new());
        store
            .create(fs_model::ROOT_INODE, "source", fs_model::S_IFREG | 0o644)
            .await
            .unwrap();
        let replaced = store
            .create(fs_model::ROOT_INODE, "target", fs_model::S_IFREG | 0o644)
            .await
            .unwrap();
        handle_write_bytes(
            1010,
            replaced,
            0,
            b"replaced data".to_vec(),
            "TEST_WRITE",
            &store,
            &object_store,
        )
        .await;
        let key = store
            .read_slices(replaced, 0)
            .await
            .unwrap()
            .pop()
            .unwrap()
            .block_key(0);

        let rename = raw_rename_req(
            1011,
            fs_model::ROOT_INODE,
            "source",
            fs_model::ROOT_INODE,
            "target",
        );
        let response = build_response(&rename, &store, &object_store).await;
        assert_eq!(response.opcode, abi::OP_RESULT_OK);
        assert!(matches!(
            mem_objects.get(&key).await,
            Err(object_store::ObjectStoreError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn truncate_cow_deletes_only_blocks_with_no_remaining_reference() {
        let mem_objects = object_store::MemObjectStore::new();
        let object_store: Arc<dyn ObjectStore> = Arc::new(mem_objects.clone());
        let store: Arc<dyn MetaStore> = Arc::new(MemStore::new());
        let inode = store
            .create(fs_model::ROOT_INODE, "gc-cow", fs_model::S_IFREG | 0o644)
            .await
            .unwrap();

        handle_write_bytes(
            1020,
            inode,
            0,
            vec![b'A'; 100],
            "TEST_WRITE",
            &store,
            &object_store,
        )
        .await;
        handle_write_bytes(
            1021,
            inode,
            80,
            vec![b'B'; 20],
            "TEST_WRITE",
            &store,
            &object_store,
        )
        .await;
        let slices = store.read_slices(inode, 0).await.unwrap();
        let older_key = slices[0].block_key(0);
        let newer_key = slices[1].block_key(0);

        let mut truncate = KestrelfsEvent::zeroed(abi::OP_TRUNCATE, 1022);
        truncate.payload[0..8].copy_from_slice(&inode.to_le_bytes());
        truncate.payload[8..16].copy_from_slice(&80u64.to_le_bytes());
        let response = build_response(&truncate, &store, &object_store).await;
        assert_eq!(response.opcode, abi::OP_RESULT_OK);
        assert!(mem_objects.get(&older_key).await.is_ok());
        assert!(matches!(
            mem_objects.get(&newer_key).await,
            Err(object_store::ObjectStoreError::NotFound(_))
        ));
        let remaining = store.read_slices(inode, 0).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].length, 80);

        // Extending again exposes zeros rather than bytes discarded at EOF.
        store.truncate(inode, 100).await.unwrap();
        let data = read_from_slices(inode, 75, 25, &store, &object_store)
            .await
            .unwrap();
        assert_eq!(&data[..5], &[b'A'; 5]);
        assert_eq!(&data[5..], &[0; 20]);
    }
}
