//! FUSE over io_uring (ABI 7.42, Linux 6.14 and later)
//!
//! Once `FUSE_OVER_IO_URING` is negotiated the kernel holds every new request back until each
//! of its queues, one per possible CPU, has a registered entry, and from then on delivers
//! requests into the buffers of those entries rather than through `read(2)` on `/dev/fuse`.
//! `FORGET` and `INTERRUPT` still go through `/dev/fuse`, as does every notification.
//!
//! A failed registration makes the kernel give up on io_uring for the connection and serve it
//! through `/dev/fuse` instead, so a ring fuser cannot set up is backed out of by failing a
//! registration on purpose.
//!
//! An entry has two buffers. The header holds the `fuse_in_header`, the opcode's fixed-size
//! argument, and the payload size; the payload holds everything else. A reply goes back
//! through the same two buffers, and committing it fetches the entry's next request in the
//! same submission.
//!
//! Each queue is served by one thread that owns its ring, so the kernel copies requests in that
//! thread's context and no other thread may submit to it. A reply sent from another thread is
//! written into the entry's buffers there and handed to the queue's thread to commit.

use std::cell::UnsafeCell;
use std::io;
use std::io::IoSlice;
use std::mem::size_of;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::fd::RawFd;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::thread::JoinHandle;
use std::thread::ThreadId;

use io_uring::IoUring;
use io_uring::Probe;
use io_uring::cqueue;
use io_uring::opcode;
use io_uring::squeue;
use io_uring::types;
use log::debug;
use log::error;
use log::warn;
use parking_lot::Mutex;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

use crate::Filesystem;
use crate::channel::ChannelSender;
use crate::dev_fuse::DevFuse;
use crate::ll::fuse_abi as abi;
use crate::ll::fuse_abi::fuse_in_header;
use crate::ll::fuse_abi::fuse_out_header;
use crate::ll::fuse_abi::fuse_uring_cmd_req;
use crate::ll::fuse_abi::fuse_uring_ent_in_out;
use crate::ll::fuse_abi::fuse_uring_req_header;
use crate::request::RequestWithSender;
use crate::session::SessionEventLoop;

type Ring = IoUring<squeue::Entry128, cqueue::Entry>;

/// `user_data` of the read on the queue's eventfd. Entries use their index
const WAKE: u64 = u64::MAX;

/// Where `in_out` starts in the header buffer
const IN_OUT: usize = 0;
/// Where `op_in` starts in the header buffer
const OP_IN: usize = abi::FUSE_URING_IN_OUT_HEADER_SZ;
/// Where `ring_ent_in_out` starts in the header buffer
const ENT_IN_OUT: usize = OP_IN + abi::FUSE_URING_OP_IN_OUT_SZ;
const HEADER_SZ: usize = size_of::<fuse_uring_req_header>();
const IN_HEADER_SZ: usize = size_of::<fuse_in_header>();
const OUT_HEADER_SZ: usize = size_of::<fuse_out_header>();
/// Alignment the request parser needs, which is that of `fuse_in_header`
const REQUEST_ALIGN: usize = 8;
/// Offset of `len` in an `io_uring_sqe`, which `opcode::UringCmd80` has no setter for
const SQE_LEN_OFFSET: usize = 24;

/// Whether this process can run a FUSE io_uring queue: the ring setup fuser uses is allowed
/// and supports `IORING_OP_URING_CMD`. Returns the number of queues the kernel will expect.
///
/// The kernel only advertises `FUSE_OVER_IO_URING` when its own side is enabled, but seccomp
/// and the `kernel.io_uring_disabled` sysctl can still keep a process from using io_uring.
/// Negotiating it anyway would leave every request waiting for queues that are never
/// registered, so this is checked first.
pub(crate) fn probe() -> io::Result<u16> {
    let queues = possible_cpus()?;
    let ring = new_ring(2)?;
    let mut probe = Probe::new();
    ring.submitter().register_probe(&mut probe)?;
    if !probe.is_supported(opcode::UringCmd80::CODE) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "io_uring does not support IORING_OP_URING_CMD",
        ));
    }
    Ok(queues)
}

/// A ring whose task work runs only when its own thread asks for completions. The kernel
/// copies a request into an entry's buffers as task work, so this keeps that copy on the
/// queue's thread rather than interrupting whatever it is doing
fn new_ring(entries: u32) -> io::Result<Ring> {
    IoUring::builder()
        .setup_single_issuer()
        .setup_defer_taskrun()
        .dontfork()
        .build(entries)
}

/// The kernel has one queue per possible CPU, not per online one
fn possible_cpus() -> io::Result<u16> {
    let count = match std::fs::read_to_string("/sys/devices/system/cpu/possible") {
        Ok(list) => parse_cpu_list(&list).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("cannot parse possible CPUs {list:?}"),
            )
        })?,
        Err(_) => {
            // SAFETY: sysconf has no preconditions
            let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_CONF) };
            if n < 1 {
                return Err(io::Error::last_os_error());
            }
            n as usize
        }
    };
    u16::try_from(count)
        .ok()
        .filter(|&n| n > 0)
        .ok_or_else(|| io::Error::other(format!("unsupported number of CPUs: {count}")))
}

/// Counts the CPUs in a kernel CPU list such as `0-3,8,10-11`
fn parse_cpu_list(list: &str) -> Option<usize> {
    let mut count = 0usize;
    for range in list.trim().split(',') {
        let (first, last) = match range.split_once('-') {
            Some((first, last)) => (first.parse::<usize>().ok()?, last.parse::<usize>().ok()?),
            None => {
                let cpu = range.parse::<usize>().ok()?;
                (cpu, cpu)
            }
        };
        count = count.checked_add(last.checked_sub(first)?.checked_add(1)?)?;
    }
    Some(count)
}

/// Sets the `len` field of a submission queue entry
fn set_sqe_len(entry: &mut squeue::Entry128, len: u32) {
    // SAFETY: `Entry128` is `repr(C)` around the kernel's 128-byte `io_uring_sqe`, whose `len`
    // is the `u32` at this offset. A test checks the offset against an opcode that sets it
    unsafe {
        ptr::from_mut(entry)
            .cast::<u8>()
            .add(SQE_LEN_OFFSET)
            .cast::<u32>()
            .write_unaligned(len);
    }
}

fn uring_cmd(fd: RawFd, cmd_op: u32, qid: u16, commit_id: u64) -> opcode::UringCmd80 {
    let req = fuse_uring_cmd_req {
        flags: 0,
        commit_id,
        qid,
        padding: [0; 6],
    };
    let mut cmd = [0u8; 80];
    cmd[..size_of::<fuse_uring_cmd_req>()].copy_from_slice(req.as_bytes());
    opcode::UringCmd80::new(types::Fd(fd), cmd_op).cmd(cmd)
}

/// Turns io_uring off for the connection, so that the requests it is holding back for the
/// queues go through `/dev/fuse` instead. The kernel does this when a registration fails, and
/// one naming a queue that cannot exist always does
pub(crate) fn disable_in_kernel(fuse: &DevFuse) -> io::Result<()> {
    let mut ring = new_ring(2)?;
    let iov = [libc::iovec {
        iov_base: ptr::null_mut(),
        iov_len: 0,
    }; 2];
    let mut entry = uring_cmd(
        fuse.as_raw_fd(),
        abi::FUSE_IO_URING_CMD_REGISTER,
        u16::MAX,
        0,
    )
    .addr(Some(iov.as_ptr() as u64))
    .build()
    .user_data(0);
    set_sqe_len(&mut entry, 2);
    // SAFETY: the iovecs outlive the submission, which completes before this returns
    unsafe { ring.submission().push(&entry) }.map_err(io::Error::other)?;
    loop {
        match ring.submit_and_wait(1) {
            Ok(_) => break,
            Err(err) if err.raw_os_error() == Some(libc::EINTR) => continue,
            Err(err) => return Err(err),
        }
    }
    match ring.completion().next().map(|cqe| cqe.result()) {
        Some(res) if res < 0 => Ok(()),
        res => Err(io::Error::other(format!(
            "the kernel accepted a registration for a queue that cannot exist: {res:?}"
        ))),
    }
}

/// How an entry's memory is laid out
#[derive(Clone, Copy, Debug)]
struct Layout {
    /// From the start of an entry to its payload. The header buffer comes first; the space
    /// between it and the payload is where a request is reassembled in one piece
    payload_offset: usize,
    /// Size of the payload buffer, at least the largest request or reply the kernel can send
    payload_size: usize,
    /// From one entry to the next
    entry_size: usize,
}

impl Layout {
    fn new(payload_size: usize) -> Self {
        let page = page_size::get();
        // Enough room ahead of the payload to move a request with a name or a symlink target
        // out of the payload, so that a reply can be written there while the request is
        // still in use
        let payload_offset = 8192usize.next_multiple_of(page);
        let payload_size = payload_size.next_multiple_of(page);
        Self {
            payload_offset,
            payload_size,
            entry_size: payload_offset + payload_size,
        }
    }

    /// Largest request that is copied whole into the space ahead of the payload
    fn small_request_limit(&self) -> usize {
        self.payload_offset - HEADER_SZ - REQUEST_ALIGN
    }
}

/// Anonymous memory holding every entry of a queue
#[derive(Debug)]
struct Mapping {
    base: *mut u8,
    len: usize,
}

// SAFETY: the mapping is plain memory; access to each entry's part of it is serialized by that
// entry's slot
unsafe impl Send for Mapping {}
// SAFETY: as above
unsafe impl Sync for Mapping {}

impl Mapping {
    fn new(len: usize) -> io::Result<Self> {
        // Reserved rather than committed: an entry only takes memory as large requests and
        // replies pass through it
        // SAFETY: an anonymous mapping with no address hint has no preconditions
        let base = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            base: base.cast(),
            len,
        })
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: this is the mapping mmap returned, and nothing refers to it any more
        unsafe { libc::munmap(self.base.cast(), self.len) };
    }
}

/// Who may touch an entry's buffers, and what is in them
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    /// The kernel has the entry, waiting to put a request in it
    Kernel,
    /// The filesystem is being called with the request. While `payload_busy` the request still
    /// lies in the payload buffer, so a reply has to be staged until the call returns
    Dispatching { payload_busy: bool },
    /// The call returned without a reply; whoever replies writes it straight into the buffers
    Detached,
    /// A reply is in the header buffer, and in the payload buffer unless `staged`
    Replied { staged: bool },
    /// The kernel has let go of the entry for good
    Dead,
}

#[derive(Debug)]
struct Slot {
    phase: Phase,
    /// What the kernel identifies the entry's current request by
    commit_id: u64,
    /// Reply payload waiting for the request to leave the payload buffer
    staged: Vec<u8>,
}

/// The part of a queue that replies reach from any thread
#[derive(Debug)]
pub(crate) struct QueueShared {
    qid: u16,
    layout: Layout,
    mem: Mapping,
    slots: Box<[Mutex<Slot>]>,
    /// Entries replied to after their dispatch returned, for the queue's thread to commit
    ready: Mutex<Vec<u16>>,
    /// Written to wake the queue's thread for `ready` and for stopping
    wake: OwnedFd,
    owner: ThreadId,
    stopping: AtomicBool,
    /// Set once the queue's thread no longer commits anything
    closed: AtomicBool,
    /// For what a reply does through `/dev/fuse`, such as opening backing files
    ch: ChannelSender,
}

impl QueueShared {
    fn header(&self, idx: usize) -> *mut u8 {
        debug_assert!(idx < self.slots.len());
        // SAFETY: entries lie within the mapping
        unsafe { self.mem.base.add(idx * self.layout.entry_size) }
    }

    fn payload(&self, idx: usize) -> *mut u8 {
        // SAFETY: the payload lies within the entry
        unsafe { self.header(idx).add(self.layout.payload_offset) }
    }

    fn wake(&self) {
        let one = 1u64;
        // SAFETY: writes 8 bytes from a live u64 to an eventfd we own
        let res = unsafe {
            libc::write(
                self.wake.as_raw_fd(),
                ptr::from_ref(&one).cast(),
                size_of::<u64>(),
            )
        };
        // EAGAIN means the counter is saturated, which wakes the reader just as well
        if res < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EAGAIN) {
            error!(
                "waking io_uring queue {}: {}",
                self.qid,
                io::Error::last_os_error()
            );
        }
    }

    fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.wake();
    }

    /// Writes a reply for entry `idx` into its header buffer, and its payload into either the
    /// payload buffer or `staged`. `data` is a `fuse_out_header` followed by the payload
    ///
    /// # Safety
    /// The caller must have the entry's slot in a phase where it owns the buffers written to
    unsafe fn write_reply(&self, idx: usize, data: &[IoSlice<'_>], staged: Option<&mut Vec<u8>>) {
        let total: usize = data.iter().map(|d| d.len()).sum();
        let mut out = [0u8; OUT_HEADER_SZ];
        copy_from_iov(data, 0, &mut out);
        let out = fuse_out_header::read_from_bytes(&out).expect("sized to fit");
        let mut error = out.error;
        let mut body_len = total.saturating_sub(OUT_HEADER_SZ);
        if body_len > self.layout.payload_size || total < OUT_HEADER_SZ {
            error!(
                "io_uring queue {}: reply of {total} bytes does not fit in {}",
                self.qid, self.layout.payload_size
            );
            error = -libc::EIO;
        }
        if error != 0 {
            // The kernel takes the error and ignores the payload
            body_len = 0;
        }
        // SAFETY: the caller owns the header buffer
        unsafe { self.write_header(idx, error, out.unique, body_len) };
        if body_len == 0 {
            return;
        }
        match staged {
            Some(staged) => {
                staged.clear();
                staged.resize(body_len, 0);
                copy_from_iov(data, OUT_HEADER_SZ, staged);
            }
            None => {
                // SAFETY: the caller owns the payload buffer, and the body fits in it
                let payload =
                    unsafe { std::slice::from_raw_parts_mut(self.payload(idx), body_len) };
                copy_from_iov(data, OUT_HEADER_SZ, payload);
            }
        }
    }

    /// Writes the `fuse_out_header` of a reply, and the size of its payload
    ///
    /// # Safety
    /// The caller must have the entry's slot in a phase where it owns the header buffer
    unsafe fn write_header(&self, idx: usize, error: i32, unique: u64, body_len: usize) {
        let out = fuse_out_header {
            len: (OUT_HEADER_SZ + body_len) as u32,
            error,
            unique,
        };
        let header = self.header(idx);
        // SAFETY: the caller owns the header buffer, and these lie within it
        unsafe {
            ptr::copy_nonoverlapping(out.as_bytes().as_ptr(), header.add(IN_OUT), OUT_HEADER_SZ);
            header
                .add(ENT_IN_OUT + std::mem::offset_of!(fuse_uring_ent_in_out, payload_sz))
                .cast::<u32>()
                .write_unaligned(body_len as u32);
        }
    }
}

/// Copies `data`, from `skip` bytes in, into all of `dst`
fn copy_from_iov(data: &[IoSlice<'_>], mut skip: usize, dst: &mut [u8]) {
    let mut written = 0;
    for slice in data {
        if written == dst.len() {
            break;
        }
        if skip >= slice.len() {
            skip -= slice.len();
            continue;
        }
        let src = &slice[skip..];
        skip = 0;
        let n = src.len().min(dst.len() - written);
        dst[written..written + n].copy_from_slice(&src[..n]);
        written += n;
    }
}

/// Sends the reply to one request that arrived in a queue entry
#[derive(Clone, Debug)]
pub(crate) struct Replier {
    shared: Arc<QueueShared>,
    idx: u16,
}

impl Replier {
    pub(crate) fn channel(&self) -> &ChannelSender {
        &self.shared.ch
    }

    pub(crate) fn send(&self, data: &[IoSlice<'_>]) -> io::Result<()> {
        self.reply_with(|shared, idx, staged| {
            // SAFETY: `reply_with` only calls this while the reply owns the buffers written to
            unsafe { shared.write_reply(idx, data, staged) }
        })
    }

    /// Replies with a payload `fill` writes straight into the entry's payload buffer, unless
    /// the request is still in it
    pub(crate) fn send_filled(
        &self,
        unique: u64,
        size: usize,
        fill: impl FnOnce(&mut [u8]) -> Result<usize, crate::Errno>,
    ) -> io::Result<()> {
        self.reply_with(|shared, idx, staged| {
            // Nothing larger is a reply the kernel would take
            let size = size.min(shared.layout.payload_size);
            let (result, staged) = match staged {
                Some(staged) => {
                    staged.clear();
                    staged.resize(size, 0);
                    (fill(staged), Some(staged))
                }
                None => {
                    // SAFETY: `reply_with` only calls this while the reply owns the payload
                    // buffer, and `size` fits in it
                    let payload =
                        unsafe { std::slice::from_raw_parts_mut(shared.payload(idx), size) };
                    (fill(payload), None)
                }
            };
            let (error, len) = match result {
                Ok(n) if n <= size => (0, n),
                Ok(n) => {
                    error!("Filled {n} bytes of a {size} byte reply");
                    (-libc::EIO, 0)
                }
                Err(err) => (-err.code(), 0),
            };
            if let Some(staged) = staged {
                staged.truncate(len);
            }
            // SAFETY: as above, for the header buffer
            unsafe { shared.write_header(idx, error, unique, len) };
        })
    }

    /// Runs `write` to put a reply in the entry's buffers, giving it the staging buffer where
    /// the payload buffer is still taken by the request, and has the entry committed
    fn reply_with(
        &self,
        write: impl FnOnce(&QueueShared, usize, Option<&mut Vec<u8>>),
    ) -> io::Result<()> {
        let shared = &self.shared;
        let idx = usize::from(self.idx);
        let mut slot = shared.slots[idx].lock();
        if shared.closed.load(Ordering::SeqCst) {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "the io_uring queue has stopped",
            ));
        }
        // While dispatching, the queue's thread commits the reply once the dispatch returns.
        // After that, the reply is handed to it
        let (staged, handed_over) = match slot.phase {
            Phase::Dispatching { payload_busy } => (payload_busy, false),
            Phase::Detached => (false, true),
            phase => {
                return Err(io::Error::other(format!(
                    "io_uring entry in phase {phase:?} cannot take a reply"
                )));
            }
        };
        let slot_ref = &mut *slot;
        write(shared, idx, staged.then_some(&mut slot_ref.staged));
        slot.phase = Phase::Replied { staged };
        drop(slot);
        if handed_over {
            shared.ready.lock().push(self.idx);
            // The queue's thread looks at `ready` before it next waits
            if thread::current().id() != shared.owner {
                shared.wake();
            }
        }
        Ok(())
    }
}

/// A queue as seen from its thread, which alone submits to the ring
struct Queue {
    /// Declared first so that it is dropped first: closing the ring makes the kernel let go
    /// of the buffers
    ring: Ring,
    shared: Arc<QueueShared>,
    fuse_fd: RawFd,
    /// Entries the kernel has not let go of
    live: usize,
    /// Where the eventfd read lands
    wake_buf: Box<UnsafeCell<u64>>,
    /// Registration buffers, read by the kernel during registration
    iovecs: Box<[[libc::iovec; 2]]>,
    /// Entries a request arrived in while the queue was still being registered
    arrived: Vec<usize>,
}

impl Queue {
    fn new(
        qid: u16,
        depth: u16,
        layout: Layout,
        fuse: &Arc<DevFuse>,
        ch: ChannelSender,
    ) -> io::Result<Self> {
        let depth = usize::from(depth);
        let mem = Mapping::new(layout.entry_size.checked_mul(depth).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "io_uring queue too large")
        })?)?;
        // One submission per entry at most, plus the eventfd read
        let ring = new_ring(u32::try_from(depth + 1).expect("bounded queue depth"))?;
        // SAFETY: eventfd has no preconditions
        let wake = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if wake < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: eventfd returned a new descriptor that nothing else owns
        let wake = unsafe { OwnedFd::from_raw_fd(wake) };
        let slots = (0..depth)
            .map(|_| {
                Mutex::new(Slot {
                    phase: Phase::Kernel,
                    commit_id: 0,
                    staged: Vec::new(),
                })
            })
            .collect();
        let shared = Arc::new(QueueShared {
            qid,
            layout,
            mem,
            slots,
            ready: Mutex::new(Vec::with_capacity(depth)),
            wake,
            owner: thread::current().id(),
            stopping: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            ch,
        });
        let iovecs = (0..depth)
            .map(|idx| {
                [
                    libc::iovec {
                        iov_base: shared.header(idx).cast(),
                        iov_len: HEADER_SZ,
                    },
                    libc::iovec {
                        iov_base: shared.payload(idx).cast(),
                        iov_len: layout.payload_size,
                    },
                ]
            })
            .collect();
        Ok(Self {
            ring,
            shared,
            fuse_fd: fuse.as_raw_fd(),
            live: 0,
            wake_buf: Box::new(UnsafeCell::new(0)),
            iovecs,
            arrived: vec![],
        })
    }

    fn push(&mut self, entry: &squeue::Entry128) {
        // SAFETY: every buffer a submission refers to lives as long as the ring
        let pushed = unsafe { self.ring.submission().push(entry) };
        // The ring has room for one submission per entry and the eventfd read, and none has
        // more than one outstanding
        pushed.expect("io_uring submission queue sized for every entry");
    }

    fn push_register(&mut self, idx: usize) {
        let qid = self.shared.qid;
        let mut entry = uring_cmd(self.fuse_fd, abi::FUSE_IO_URING_CMD_REGISTER, qid, 0)
            .addr(Some(self.iovecs[idx].as_ptr() as u64))
            .build()
            .user_data(idx as u64);
        set_sqe_len(&mut entry, 2);
        self.push(&entry);
    }

    fn push_commit(&mut self, idx: usize, commit_id: u64) {
        let qid = self.shared.qid;
        let entry = uring_cmd(
            self.fuse_fd,
            abi::FUSE_IO_URING_CMD_COMMIT_AND_FETCH,
            qid,
            commit_id,
        )
        .build()
        .user_data(idx as u64);
        self.push(&entry);
    }

    fn push_wake_read(&mut self) {
        let entry = opcode::Read::new(
            types::Fd(self.shared.wake.as_raw_fd()),
            self.wake_buf.get().cast(),
            size_of::<u64>() as u32,
        )
        .build()
        .user_data(WAKE);
        self.push(&entry.into());
    }

    fn submit(&mut self, wait: usize) -> io::Result<()> {
        loop {
            match self.ring.submit_and_wait(wait) {
                Ok(_) => return Ok(()),
                // EBUSY: completions have to be reaped before more can be submitted
                Err(err)
                    if matches!(
                        err.raw_os_error(),
                        Some(libc::EINTR | libc::EAGAIN | libc::EBUSY)
                    ) =>
                {
                    if wait > 0 {
                        return Ok(());
                    }
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// Registers every entry. A registration that fails does so as it is submitted, and a
    /// successful one only completes once a request arrives
    fn register(&mut self) -> io::Result<()> {
        let depth = self.shared.slots.len();
        let mut pending: Vec<usize> = (0..depth).collect();
        // The kernel refuses registrations until it has processed the INIT reply, which a
        // write to /dev/fuse does before returning, so this is not expected to retry
        for attempt in 0.. {
            for &idx in &pending {
                self.push_register(idx);
            }
            self.submit(0)?;
            let mut retry = vec![];
            let completions: Vec<(u64, i32)> = self
                .ring
                .completion()
                .map(|cqe| (cqe.user_data(), cqe.result()))
                .collect();
            for (user_data, res) in completions {
                if res == -libc::EAGAIN && attempt < 50 {
                    retry.push(user_data as usize);
                } else if res < 0 {
                    return Err(io::Error::from_raw_os_error(-res));
                } else {
                    // The last registration can make the kernel's queues ready, and a request
                    // it was holding back can arrive before this has looked
                    self.arrived.push(user_data as usize);
                }
            }
            if retry.is_empty() {
                break;
            }
            pending = retry;
            thread::sleep(std::time::Duration::from_millis(10));
        }
        self.live = depth;
        self.push_wake_read();
        self.submit(0)
    }

    fn serve<FS: Filesystem>(&mut self, se: &SessionEventLoop<FS>) {
        let mut completions: Vec<(u64, i32)> = std::mem::take(&mut self.arrived)
            .into_iter()
            .map(|idx| (idx as u64, 0))
            .collect();
        completions.reserve(self.shared.slots.len() + 1);
        for (user_data, _) in completions.drain(..) {
            self.deliver(user_data as usize, se);
        }
        while self.live > 0 && !self.shared.stopping.load(Ordering::SeqCst) {
            // Commits queued since the last pass go in with the wait
            if let Err(err) = self.submit(1) {
                error!("io_uring queue {}: {err}", self.shared.qid);
                break;
            }
            completions.extend(
                self.ring
                    .completion()
                    .map(|cqe| (cqe.user_data(), cqe.result())),
            );
            for (user_data, res) in completions.drain(..) {
                if user_data == WAKE {
                    if res < 0 {
                        warn!(
                            "io_uring queue {}: reading its eventfd: {}",
                            self.shared.qid,
                            io::Error::from_raw_os_error(-res)
                        );
                    }
                    self.push_wake_read();
                } else if res < 0 {
                    self.entry_lost(user_data as usize, res);
                } else {
                    self.deliver(user_data as usize, se);
                }
            }
            self.commit_ready();
        }
        debug!("io_uring queue {} stopped", self.shared.qid);
    }

    fn entry_lost(&mut self, idx: usize, res: i32) {
        let err = io::Error::from_raw_os_error(-res);
        // These are how the kernel hands entries back when the connection ends
        if matches!(-res, libc::ENOTCONN | libc::ECONNABORTED | libc::ECANCELED) {
            debug!("io_uring queue {} entry {idx}: {err}", self.shared.qid);
        } else {
            error!("io_uring queue {} entry {idx}: {err}", self.shared.qid);
        }
        self.shared.slots[idx].lock().phase = Phase::Dead;
        self.live -= 1;
    }

    fn commit_ready(&mut self) {
        let ready = std::mem::take(&mut *self.shared.ready.lock());
        for idx in ready {
            let idx = usize::from(idx);
            let commit_id = {
                let mut slot = self.shared.slots[idx].lock();
                debug_assert_eq!(slot.phase, Phase::Replied { staged: false });
                slot.phase = Phase::Kernel;
                slot.commit_id
            };
            self.push_commit(idx, commit_id);
        }
    }

    /// Hands the request that arrived in entry `idx` to the filesystem
    fn deliver<FS: Filesystem>(&mut self, idx: usize, se: &SessionEventLoop<FS>) {
        let shared = self.shared.clone();
        let header = shared.header(idx);
        // SAFETY: the kernel handed the entry over with this completion, so its buffers are
        // ours until it is committed
        let (in_header, ent_in_out) = unsafe {
            let mut in_bytes = [0u8; IN_HEADER_SZ];
            ptr::copy_nonoverlapping(header.add(IN_OUT), in_bytes.as_mut_ptr(), IN_HEADER_SZ);
            (
                fuse_in_header::read_from_bytes(&in_bytes).expect("sized to fit"),
                header
                    .add(ENT_IN_OUT)
                    .cast::<fuse_uring_ent_in_out>()
                    .read_unaligned(),
            )
        };
        let unique = in_header.unique;
        let opcode = in_header.opcode;
        let mut heap = Vec::new();
        let assembled = assemble(
            &shared.layout,
            &in_header,
            ent_in_out.payload_sz as usize,
            header,
            &mut heap,
        );
        {
            let mut slot = shared.slots[idx].lock();
            slot.commit_id = ent_in_out.commit_id;
            slot.phase = Phase::Dispatching {
                payload_busy: matches!(assembled, Ok(Assembled::InPlace { .. })),
            };
        }
        let replier = Replier {
            shared: shared.clone(),
            idx: idx as u16,
        };
        let data: Option<&[u8]> = match assembled {
            // SAFETY: the assembled request lies within the entry, which is not written to
            // until the request is done with, except for the parts a reply owns
            Ok(Assembled::InPlace { start, len } | Assembled::Copied { start, len }) => {
                Some(unsafe { std::slice::from_raw_parts(header.add(start), len) })
            }
            Ok(Assembled::Heap) => Some(as_bytes(&heap, in_header.len as usize)),
            Err(err) => {
                error!("io_uring queue {}: {err}", shared.qid);
                None
            }
        };
        let request = data.and_then(|data| {
            RequestWithSender::new_uring(se.ch.sender(), data, se.negotiated, replier.clone())
        });
        match request {
            Some(request) => {
                // A queue whose thread died would leave every later request from its CPU
                // waiting forever, so a panic costs only the request that caused it, which is
                // answered with EIO as its reply is dropped
                let served = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if opcode == abi::fuse_opcode::FUSE_DESTROY as u32 {
                        // As on /dev/fuse, the filesystem finishes its work before the reply
                        // lets a block-backed mount release its device
                        if let Some(fs) = &se.filesystem.fs {
                            fs.before_destroy();
                        }
                        // The connection is going away; nothing is torn down until it has
                        request.reply::<crate::ReplyEmpty>().ok();
                    } else {
                        request.dispatch(se);
                    }
                }));
                if served.is_err() {
                    error!(
                        "io_uring queue {}: the filesystem panicked serving request {unique}",
                        shared.qid
                    );
                }
            }
            None => {
                let out = fuse_out_header {
                    len: OUT_HEADER_SZ as u32,
                    error: -libc::EIO,
                    unique,
                };
                let _ = replier.send(&[IoSlice::new(out.as_bytes())]);
            }
        }
        drop(heap);
        self.finish(idx, opcode, unique);
    }

    /// Commits the entry if the request has been replied to, and otherwise leaves it to the
    /// reply to come
    fn finish(&mut self, idx: usize, opcode: u32, unique: u64) {
        let shared = self.shared.clone();
        let mut slot = shared.slots[idx].lock();
        match slot.phase {
            Phase::Replied { staged } => {
                if staged && !slot.staged.is_empty() {
                    // SAFETY: the request is done with, so the payload buffer is free, and the
                    // staged reply was checked to fit when it was written
                    unsafe {
                        ptr::copy_nonoverlapping(
                            slot.staged.as_ptr(),
                            shared.payload(idx),
                            slot.staged.len(),
                        );
                    }
                    // Keep small allocations for reuse, but not one a large reply left behind
                    if slot.staged.capacity() > 64 * 1024 {
                        slot.staged = Vec::new();
                    }
                }
                slot.phase = Phase::Kernel;
                let commit_id = slot.commit_id;
                drop(slot);
                self.push_commit(idx, commit_id);
            }
            Phase::Dispatching { .. } if is_unanswered(opcode) => {
                // The kernel keeps the entry until something is committed for it
                let out = fuse_out_header {
                    len: OUT_HEADER_SZ as u32,
                    error: 0,
                    unique,
                };
                // SAFETY: the dispatch returned, so both buffers are ours
                unsafe { shared.write_reply(idx, &[IoSlice::new(out.as_bytes())], None) };
                slot.phase = Phase::Kernel;
                let commit_id = slot.commit_id;
                drop(slot);
                self.push_commit(idx, commit_id);
            }
            Phase::Dispatching { .. } => slot.phase = Phase::Detached,
            phase => error!(
                "io_uring queue {} entry {idx}: unexpected phase {phase:?} after dispatch",
                shared.qid
            ),
        }
    }
}

/// Requests the filesystem is not asked to answer. The kernel sends these through `/dev/fuse`,
/// but an entry that received one would otherwise never be committed
fn is_unanswered(opcode: u32) -> bool {
    opcode == abi::fuse_opcode::FUSE_FORGET as u32
        || opcode == abi::fuse_opcode::FUSE_BATCH_FORGET as u32
}

fn as_bytes(words: &[u64], len: usize) -> &[u8] {
    &words.as_bytes()[..len]
}

#[derive(Debug, Eq, PartialEq)]
enum Assembled {
    /// The request ends where the payload buffer does, still holding the payload
    InPlace { start: usize, len: usize },
    /// The whole request was copied ahead of the payload buffer
    Copied { start: usize, len: usize },
    /// The request is in the heap buffer
    Heap,
}

/// Puts the request in an entry back together as `/dev/fuse` would have delivered it: the
/// `fuse_in_header`, the opcode's argument from `op_in`, and then the payload. A small request
/// is copied whole ahead of the payload buffer, freeing that for the reply; a large one is
/// completed in front of its payload so that the payload is not copied. Offsets are from the
/// start of the entry
fn assemble(
    layout: &Layout,
    in_header: &fuse_in_header,
    payload_sz: usize,
    entry: *const u8,
    heap: &mut Vec<u64>,
) -> io::Result<Assembled> {
    let len = in_header.len as usize;
    let op_len = len
        .checked_sub(IN_HEADER_SZ + payload_sz)
        .filter(|&op_len| {
            op_len <= abi::FUSE_URING_OP_IN_OUT_SZ && payload_sz <= layout.payload_size
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "request of {len} bytes with a payload of {payload_sz} does not fit an entry"
                ),
            )
        })?;
    let prefix = IN_HEADER_SZ + op_len;
    let copy = |dst: *mut u8| {
        // SAFETY: the sources lie within the entry, and `dst` has room for `len` bytes that do
        // not overlap them
        unsafe {
            ptr::copy_nonoverlapping(entry.add(IN_OUT), dst, IN_HEADER_SZ);
            ptr::copy_nonoverlapping(entry.add(OP_IN), dst.add(IN_HEADER_SZ), op_len);
        }
    };
    let entry_mut = entry.cast_mut();
    if len <= layout.small_request_limit() {
        let start = (layout.payload_offset - len) / REQUEST_ALIGN * REQUEST_ALIGN;
        // SAFETY: [start, start + len) lies between the header buffer and the payload
        unsafe {
            let dst = entry_mut.add(start);
            copy(dst);
            ptr::copy(
                entry.add(layout.payload_offset),
                dst.add(prefix),
                payload_sz,
            );
        }
        return Ok(Assembled::Copied { start, len });
    }
    let start = layout.payload_offset - prefix;
    if start % REQUEST_ALIGN == 0 {
        // SAFETY: the prefix fits between the header buffer and the payload
        copy(unsafe { entry_mut.add(start) });
        return Ok(Assembled::InPlace { start, len });
    }
    // No opcode has an argument that leaves the payload misaligned
    heap.clear();
    heap.resize(len.div_ceil(size_of::<u64>()), 0);
    let dst = heap.as_mut_ptr().cast::<u8>();
    copy(dst);
    // SAFETY: the heap buffer has room for the whole request
    unsafe {
        ptr::copy_nonoverlapping(
            entry.add(layout.payload_offset),
            dst.add(prefix),
            payload_sz,
        );
    }
    Ok(Assembled::Heap)
}

/// What a session needs to bring its io_uring queues up
#[derive(Clone, Copy, Debug)]
pub(crate) struct QueueParams {
    pub(crate) queues: u16,
    pub(crate) depth: u16,
    /// The largest request or reply payload the kernel can exchange with an entry
    pub(crate) payload_size: usize,
    /// A queue whose registration is made to fail
    #[cfg(test)]
    pub(crate) fail_queue: Option<u16>,
}

impl QueueParams {
    /// The kernel requires every entry's payload buffer to hold `max(8 KiB, max_write,
    /// max_pages * page size)`, where it may lower `max_pages` from what was asked for but
    /// never raises it
    pub(crate) fn payload_size(max_write: u32, max_pages: u16) -> usize {
        // FUSE_MIN_READ_BUFFER in the kernel
        const MIN: usize = 8192;
        MIN.max(max_write as usize)
            .max(usize::from(max_pages) * page_size::get())
    }
}

/// The running queues of a session
#[derive(Debug)]
pub(crate) struct Queues {
    shared: Vec<Arc<QueueShared>>,
    threads: Vec<JoinHandle<()>>,
}

impl Queues {
    /// Starts one thread per queue, each serving with one of `loops`, and returns once every
    /// entry is registered. On failure every queue is stopped again, and the caller has to
    /// decide whether to fall back to `/dev/fuse`
    pub(crate) fn start<FS: Filesystem>(
        params: QueueParams,
        fuse: &Arc<DevFuse>,
        loops: Vec<SessionEventLoop<FS>>,
    ) -> io::Result<Queues> {
        debug_assert_eq!(loops.len(), usize::from(params.queues));
        let layout = Layout::new(params.payload_size);
        let (report, reports) = mpsc::channel();
        let mut queues = Queues {
            shared: vec![],
            threads: vec![],
        };
        let mut first_error = None;
        for (qid, se) in loops.into_iter().enumerate() {
            let qid = qid as u16;
            let fuse = fuse.clone();
            let report = report.clone();
            let spawned = thread::Builder::new()
                .name(format!("fuser-uring-{qid}"))
                .spawn(move || {
                    pin_to_cpu(usize::from(qid));
                    // Every queue reports, or the session would wait for it forever
                    let queue = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let mut queue =
                            Queue::new(qid, params.depth, layout, &fuse, se.ch.sender())?;
                        #[cfg(test)]
                        if params.fail_queue == Some(qid) {
                            return Err(io::Error::other("injected failure"));
                        }
                        queue.register()?;
                        Ok(queue)
                    }))
                    .unwrap_or_else(|_| Err(io::Error::other("panicked")));
                    match queue {
                        Ok(mut queue) => {
                            let _ = report.send(Ok(queue.shared.clone()));
                            queue.serve(&se);
                            queue.shared.closed.store(true, Ordering::SeqCst);
                        }
                        Err(err) => {
                            let _ = report.send(Err(io::Error::new(
                                err.kind(),
                                format!("setting up io_uring queue {qid}: {err}"),
                            )));
                        }
                    }
                });
            match spawned {
                Ok(thread) => queues.threads.push(thread),
                Err(err) => {
                    first_error = Some(err);
                    break;
                }
            }
        }
        drop(report);
        for _ in 0..queues.threads.len() {
            match reports.recv() {
                Ok(Ok(shared)) => queues.shared.push(shared),
                Ok(Err(err)) => {
                    first_error.get_or_insert(err);
                }
                Err(_) => {
                    first_error.get_or_insert(io::Error::other("io_uring queue thread panicked"));
                    break;
                }
            }
        }
        match first_error {
            None => Ok(queues),
            Some(err) => {
                let _ = queues.stop_and_join();
                Err(err)
            }
        }
    }

    /// Stops every queue and waits for their threads. Replies still outstanding are dropped
    pub(crate) fn stop_and_join(mut self) -> io::Result<()> {
        self.stop_and_join_mut()
    }

    fn stop_and_join_mut(&mut self) -> io::Result<()> {
        for shared in &self.shared {
            shared.stop();
        }
        let mut result = Ok(());
        for thread in self.threads.drain(..) {
            if thread.join().is_err() {
                result = Err(io::Error::other("io_uring queue thread panicked"));
            }
        }
        result
    }
}

/// A session that ends early, such as by failing to start its other threads, still stops its
/// queues
impl Drop for Queues {
    fn drop(&mut self) {
        if let Err(err) = self.stop_and_join_mut() {
            error!("{err}");
        }
    }
}

/// Serves the queue of a CPU from that CPU, where the process may run there
fn pin_to_cpu(cpu: usize) {
    // CPU_SET panics beyond the set's capacity
    if cpu >= libc::CPU_SETSIZE as usize {
        return;
    }
    // SAFETY: cpu_set_t is plain data
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        if libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set) != 0 {
            debug!(
                "not pinning io_uring queue {cpu}: {}",
                io::Error::last_os_error()
            );
        }
    }
}

#[cfg(test)]
mod mount_test;

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn cpu_lists() {
        assert_eq!(parse_cpu_list("0\n"), Some(1));
        assert_eq!(parse_cpu_list("0-3\n"), Some(4));
        assert_eq!(parse_cpu_list("0-3,8,10-11"), Some(7));
        assert_eq!(parse_cpu_list("0-255"), Some(256));
        assert_eq!(parse_cpu_list(""), None);
        assert_eq!(parse_cpu_list("3-1"), None);
        assert_eq!(parse_cpu_list("a"), None);
    }

    /// `set_sqe_len` writes where the kernel reads `len`, which `Readv` sets to its iovec count
    #[test]
    fn sqe_len_offset() {
        let readv: squeue::Entry128 = opcode::Readv::new(types::Fd(7), ptr::null(), 0x0102_0304)
            .build()
            .into();
        let mut cmd = opcode::UringCmd80::new(types::Fd(7), 0).build();
        set_sqe_len(&mut cmd, 0x0102_0304);
        // SAFETY: both are 128 bytes of plain data
        let (readv, cmd) = unsafe {
            (
                std::mem::transmute::<squeue::Entry128, [u8; 128]>(readv),
                std::mem::transmute::<squeue::Entry128, [u8; 128]>(cmd),
            )
        };
        assert_eq!(
            readv[SQE_LEN_OFFSET..SQE_LEN_OFFSET + 4],
            cmd[SQE_LEN_OFFSET..SQE_LEN_OFFSET + 4]
        );
    }

    #[test]
    fn payload_size_covers_the_kernels_requirement() {
        let page = page_size::get();
        assert_eq!(QueueParams::payload_size(4096, 1), 8192);
        assert_eq!(QueueParams::payload_size(1 << 20, 1), 1 << 20);
        assert_eq!(QueueParams::payload_size(4096, 256), 256 * page);
    }

    /// Lays a request out the way the kernel does: in header and op argument in the header
    /// buffer, the rest in the payload buffer
    fn entry_with(
        layout: &Layout,
        opcode: u32,
        op: &[u8],
        payload: &[u8],
    ) -> (Vec<u64>, fuse_in_header) {
        let mut mem = vec![0u64; layout.entry_size / 8];
        let in_header = fuse_in_header {
            len: (IN_HEADER_SZ + op.len() + payload.len()) as u32,
            opcode,
            unique: 42,
            nodeid: 1,
            uid: 0,
            gid: 0,
            pid: 0,
            padding: 0,
        };
        let bytes = mem.as_mut_bytes();
        bytes[IN_OUT..IN_OUT + IN_HEADER_SZ]
            .copy_from_slice(in_header_bytes(&in_header).as_slice());
        bytes[OP_IN..OP_IN + op.len()].copy_from_slice(op);
        bytes[layout.payload_offset..layout.payload_offset + payload.len()]
            .copy_from_slice(payload);
        (mem, in_header)
    }

    fn in_header_bytes(h: &fuse_in_header) -> Vec<u8> {
        let mut v = vec![];
        v.extend_from_slice(&h.len.to_ne_bytes());
        v.extend_from_slice(&h.opcode.to_ne_bytes());
        v.extend_from_slice(&h.unique.to_ne_bytes());
        v.extend_from_slice(&h.nodeid.to_ne_bytes());
        v.extend_from_slice(&h.uid.to_ne_bytes());
        v.extend_from_slice(&h.gid.to_ne_bytes());
        v.extend_from_slice(&h.pid.to_ne_bytes());
        v.extend_from_slice(&h.padding.to_ne_bytes());
        v
    }

    fn assembled_bytes<'a>(
        mem: &'a [u64],
        heap: &'a [u64],
        in_header: &fuse_in_header,
        assembled: &Assembled,
    ) -> &'a [u8] {
        match *assembled {
            Assembled::InPlace { start, len } | Assembled::Copied { start, len } => {
                &mem.as_bytes()[start..start + len]
            }
            Assembled::Heap => as_bytes(heap, in_header.len as usize),
        }
    }

    #[test]
    fn small_requests_move_out_of_the_payload() {
        let layout = Layout::new(1 << 20);
        // A lookup has no op argument and the name as payload
        let (mut mem, in_header) = entry_with(&layout, 1, &[], b"name\0");
        let mut heap = vec![];
        let assembled =
            assemble(&layout, &in_header, 5, mem.as_mut_ptr().cast(), &mut heap).unwrap();
        assert!(matches!(assembled, Assembled::Copied { start, .. } if start % REQUEST_ALIGN == 0));
        let data = assembled_bytes(&mem, &heap, &in_header, &assembled);
        assert_eq!(
            &data[..IN_HEADER_SZ],
            in_header_bytes(&in_header).as_slice()
        );
        assert_eq!(&data[IN_HEADER_SZ..], b"name\0");
        // The request parses as it would from /dev/fuse
        let request = crate::ll::AnyRequest::try_from(data).unwrap();
        assert_eq!(request.unique().0, 42);
    }

    #[test]
    fn large_requests_keep_their_payload_in_place() {
        let layout = Layout::new(1 << 20);
        let op = [7u8; 40];
        let payload = vec![9u8; 64 * 1024];
        let (mut mem, in_header) = entry_with(&layout, 16, &op, &payload);
        let mut heap = vec![];
        let assembled = assemble(
            &layout,
            &in_header,
            payload.len(),
            mem.as_mut_ptr().cast(),
            &mut heap,
        )
        .unwrap();
        assert_eq!(
            assembled,
            Assembled::InPlace {
                start: layout.payload_offset - IN_HEADER_SZ - op.len(),
                len: in_header.len as usize
            }
        );
        let data = assembled_bytes(&mem, &heap, &in_header, &assembled);
        assert_eq!(&data[IN_HEADER_SZ..IN_HEADER_SZ + op.len()], &op);
        assert_eq!(&data[IN_HEADER_SZ + op.len()..], payload.as_slice());
    }

    #[test]
    fn misaligned_large_requests_go_to_the_heap() {
        let layout = Layout::new(1 << 20);
        let op = [7u8; 4];
        let payload = vec![9u8; 64 * 1024];
        let (mut mem, in_header) = entry_with(&layout, 16, &op, &payload);
        let mut heap = vec![];
        let assembled = assemble(
            &layout,
            &in_header,
            payload.len(),
            mem.as_mut_ptr().cast(),
            &mut heap,
        )
        .unwrap();
        assert_eq!(assembled, Assembled::Heap);
        let data = assembled_bytes(&mem, &heap, &in_header, &assembled);
        assert_eq!(data.len(), in_header.len as usize);
        assert_eq!(&data[IN_HEADER_SZ..IN_HEADER_SZ + 4], &op);
        assert_eq!(&data[IN_HEADER_SZ + 4..], payload.as_slice());
    }

    #[test]
    fn inconsistent_sizes_are_refused() {
        let layout = Layout::new(1 << 20);
        let (mut mem, mut in_header) = entry_with(&layout, 1, &[], b"x\0");
        let mut heap = vec![];
        // A payload larger than the request
        assert!(assemble(&layout, &in_header, 100, mem.as_mut_ptr().cast(), &mut heap).is_err());
        // An op argument larger than op_in
        in_header.len = (IN_HEADER_SZ + 200) as u32;
        assert!(assemble(&layout, &in_header, 0, mem.as_mut_ptr().cast(), &mut heap).is_err());
    }

    #[test]
    fn copy_from_iov_skips_and_spans_slices() {
        let a = [1u8, 2, 3];
        let b = [4u8, 5];
        let c = [6u8];
        let iov = [IoSlice::new(&a), IoSlice::new(&b), IoSlice::new(&c)];
        let mut dst = [0u8; 4];
        copy_from_iov(&iov, 2, &mut dst);
        assert_eq!(dst, [3, 4, 5, 6]);
        let mut dst = [0u8; 2];
        copy_from_iov(&iov, 0, &mut dst);
        assert_eq!(dst, [1, 2]);
    }
}
