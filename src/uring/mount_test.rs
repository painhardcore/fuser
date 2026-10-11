//! FUSE over io_uring against the running kernel. Each test is skipped where the kernel does
//! not offer it or the process cannot use io_uring.

use std::ffi::OsStr;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::mem::ManuallyDrop;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use std::time::SystemTime;

use crate::BackgroundSession;
use crate::Config;
use crate::Errno;
use crate::FileAttr;
use crate::FileHandle;
use crate::FileType;
use crate::Filesystem;
use crate::FopenFlags;
use crate::Generation;
use crate::INodeNo;
use crate::IoUringConfig;
use crate::IoUringMode;
use crate::LockOwner;
use crate::OpenFlags;
use crate::ReplyAttr;
use crate::ReplyData;
use crate::ReplyEntry;
use crate::ReplyOpen;
use crate::ReplyWrite;
use crate::Request;
use crate::Session;
use crate::WriteFlags;

const FILE: INodeNo = INodeNo(2);
/// Replied to from another thread
const SLOW: INodeNo = INodeNo(3);
/// Panics in getattr
const PANIC: INodeNo = INodeNo(4);
/// Its getattr reply is kept until something else answers it
const HELD: INodeNo = INodeNo(5);
/// Its getattr blocks until the gate opens
const BLOCKED: INodeNo = INodeNo(6);
const FILE_SIZE: u64 = 4 << 20;

/// What a byte at `pos` of `FILE` holds
fn pattern(pos: u64) -> u8 {
    (pos % 251) as u8
}

fn uring_usable() -> bool {
    let enabled = std::fs::read_to_string("/sys/module/fuse/parameters/enable_uring")
        .is_ok_and(|v| v.trim() == "Y");
    if !enabled {
        eprintln!("skipping: the fuse module's enable_uring parameter is not set");
        return false;
    }
    if let Err(err) = super::probe() {
        eprintln!("skipping: io_uring is unavailable: {err}");
        return false;
    }
    true
}

fn attr(ino: INodeNo, kind: FileType, size: u64) -> FileAttr {
    FileAttr {
        ino,
        size,
        blocks: size.div_ceil(512),
        atime: SystemTime::UNIX_EPOCH,
        mtime: SystemTime::UNIX_EPOCH,
        ctime: SystemTime::UNIX_EPOCH,
        crtime: SystemTime::UNIX_EPOCH,
        kind,
        perm: if kind == FileType::Directory {
            0o755
        } else {
            0o644
        },
        nlink: 1,
        uid: 0,
        gid: 0,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

#[derive(Default)]
struct State {
    /// The thread that served each request
    served_by: StdMutex<Vec<String>>,
    /// Bytes written that did not hold the pattern by the time they were checked
    corrupt_writes: AtomicUsize,
    bytes_written: AtomicU64,
    held: StdMutex<Option<ReplyAttr>>,
    held_cond: Condvar,
    /// Whether a getattr of `BLOCKED` has started, and whether it may finish
    gate: StdMutex<(bool, bool)>,
    gate_cond: Condvar,
}

struct TestFs(Arc<State>);

impl TestFs {
    fn record(&self) {
        let name = thread::current().name().unwrap_or("").to_string();
        self.0.served_by.lock().unwrap().push(name);
    }
}

impl Filesystem for TestFs {
    fn lookup(&self, _req: &Request, _parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        self.record();
        let ino = match name.to_str() {
            Some("file") => FILE,
            Some("slow") => SLOW,
            Some("panic") => PANIC,
            Some("held") => HELD,
            Some("blocked") => BLOCKED,
            _ => return reply.error(Errno::ENOENT),
        };
        reply.entry(
            &Duration::ZERO,
            &attr(ino, FileType::RegularFile, FILE_SIZE),
            Generation(0),
        );
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        self.record();
        let kind = if ino == INodeNo::ROOT {
            FileType::Directory
        } else {
            FileType::RegularFile
        };
        let attr = attr(ino, kind, FILE_SIZE);
        match ino {
            SLOW => {
                thread::spawn(move || {
                    thread::sleep(Duration::from_millis(1));
                    reply.attr(&Duration::ZERO, &attr);
                });
            }
            PANIC => panic!("deliberate panic serving getattr"),
            HELD => {
                *self.0.held.lock().unwrap() = Some(reply);
                self.0.held_cond.notify_all();
            }
            BLOCKED => {
                let mut gate = self.0.gate.lock().unwrap();
                gate.0 = true;
                self.0.gate_cond.notify_all();
                while !gate.1 {
                    gate = self.0.gate_cond.wait(gate).unwrap();
                }
                reply.attr(&Duration::ZERO, &attr);
            }
            _ => {
                // Answering a reply kept from an earlier request comes first, so that it is
                // sent while this request is still being served
                if let Some(held) = self.0.held.lock().unwrap().take() {
                    held.attr(&Duration::ZERO, &self::attr(HELD, kind, FILE_SIZE));
                }
                reply.attr(&Duration::ZERO, &attr);
            }
        }
    }

    fn open(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _flags: OpenFlags,
        _kill_suid_gid: bool,
        reply: ReplyOpen,
    ) {
        self.record();
        // Every read and write reaches the filesystem at the size it was issued
        reply.opened(FileHandle(0), FopenFlags::FOPEN_DIRECT_IO);
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        self.record();
        let end = (offset + u64::from(size)).min(FILE_SIZE);
        let data: Vec<u8> = (offset..end.max(offset)).map(pattern).collect();
        let fill = move |buf: &mut [u8]| {
            buf[..data.len()].copy_from_slice(&data);
            Ok(data.len())
        };
        // Each 512 KiB is answered a different way: copied or filled in place, from the
        // queue's thread or from another
        match (offset >> 19) % 4 {
            0 => reply.data(&fill_vec(fill)),
            1 => drop(thread::spawn(move || reply.data(&fill_vec(fill)))),
            2 => reply.fill(size as usize, fill),
            _ => drop(thread::spawn(move || reply.fill(size as usize, fill))),
        }
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        self.record();
        // Replying before looking at the data is allowed, and the data must survive it
        reply.written(data.len() as u32);
        let corrupt = data
            .iter()
            .zip(offset..)
            .filter(|&(&byte, pos)| byte != pattern(pos))
            .count();
        self.0.corrupt_writes.fetch_add(corrupt, Ordering::SeqCst);
        self.0
            .bytes_written
            .fetch_add(data.len() as u64, Ordering::SeqCst);
    }
}

fn fill_vec(fill: impl FnOnce(&mut [u8]) -> Result<usize, Errno>) -> Vec<u8> {
    let mut buf = vec![0; 1 << 20];
    let n = fill(&mut buf).unwrap();
    buf.truncate(n);
    buf
}

struct Mounted {
    state: Arc<State>,
    mountpoint: PathBuf,
    session: Option<BackgroundSession>,
    tmp: ManuallyDrop<tempfile::TempDir>,
}

impl Mounted {
    fn new(mode: IoUringMode, fail_queue: Option<u16>) -> io::Result<Self> {
        let defaults = IoUringConfig::default();
        Self::with_queues(
            mode,
            fail_queue,
            defaults.queue_depth,
            defaults.threads_per_queue,
        )
    }

    fn with_queues(
        mode: IoUringMode,
        fail_queue: Option<u16>,
        queue_depth: u16,
        threads_per_queue: u16,
    ) -> io::Result<Self> {
        let state = Arc::new(State::default());
        let tmp = ManuallyDrop::new(tempfile::tempdir().unwrap());
        let mountpoint = tmp.path().canonicalize().unwrap();
        let mut config = Config::default();
        config.io_uring.mode = mode;
        config.io_uring.queue_depth = queue_depth;
        config.io_uring.threads_per_queue = threads_per_queue;
        let mut session = Session::new(TestFs(state.clone()), &mountpoint, &config)?;
        if let Some(params) = &mut session.io_uring {
            params.fail_queue = fail_queue;
        }
        Ok(Self {
            state,
            mountpoint,
            session: Some(session.spawn()?),
            tmp,
        })
    }

    fn path(&self, name: &str) -> PathBuf {
        self.mountpoint.join(name)
    }

    fn served_by(&self) -> Vec<String> {
        std::mem::take(&mut *self.state.served_by.lock().unwrap())
    }

    fn unmount(mut self) -> io::Result<()> {
        let result = self.session.take().unwrap().umount_and_join();
        unsafe { ManuallyDrop::drop(&mut self.tmp) };
        result
    }
}

/// CPUs this process may run on
fn allowed_cpus() -> Vec<usize> {
    // SAFETY: cpu_set_t is plain data, filled in by the kernel
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        assert_eq!(
            libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &mut set),
            0
        );
        (0..libc::CPU_SETSIZE as usize)
            .filter(|&cpu| libc::CPU_ISSET(cpu, &set))
            .collect()
    }
}

/// Runs `f` on a thread that only runs on `cpu`
fn on_cpu<T: Send + 'static>(cpu: usize, f: impl FnOnce() -> T + Send + 'static) -> T {
    thread::spawn(move || {
        super::pin_to_cpu(cpu);
        f()
    })
    .join()
    .unwrap()
}

fn read_exact_at(path: &Path, offset: u64, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    File::open(path)
        .unwrap()
        .read_exact_at(&mut buf, offset)
        .unwrap();
    buf
}

/// Every CPU's queue serves the requests issued there, with large payloads intact both ways
#[test]
fn serves_requests_from_every_cpu() {
    if !uring_usable() {
        return;
    }
    let mounted = Mounted::new(IoUringMode::Required, None).unwrap();
    let file = mounted.path("file");
    for cpu in allowed_cpus() {
        let file = file.clone();
        on_cpu(cpu, move || {
            assert_eq!(std::fs::metadata(&file).unwrap().len(), FILE_SIZE);
            // Every way a read is answered
            for offset in (0..4).map(|i| i * (512 << 10)) {
                let data = read_exact_at(&file, offset, 512 << 10);
                for (pos, &byte) in (offset..).zip(&data) {
                    assert_eq!(byte, pattern(pos), "byte {pos}");
                }
            }
            let handle = OpenOptions::new().write(true).open(&file).unwrap();
            // Large enough that the request is served where its payload arrived
            let big: Vec<u8> = (0..1u64 << 20).map(pattern).collect();
            handle.write_all_at(&big, 0).unwrap();
            // Small enough to be moved out of the way of the reply
            let small: Vec<u8> = (4096..4196u64).map(pattern).collect();
            handle.write_all_at(&small, 4096).unwrap();
        });
        let served_by = mounted.served_by();
        assert!(
            served_by
                .iter()
                .all(|name| name.starts_with("fuser-uring-")),
            "served over /dev/fuse: {served_by:?}"
        );
        assert!(
            served_by
                .iter()
                .any(|name| name.starts_with(&format!("fuser-uring-{cpu}-"))),
            "nothing from CPU {cpu} reached its queue: {served_by:?}"
        );
    }
    assert_eq!(mounted.state.corrupt_writes.load(Ordering::SeqCst), 0);
    assert_eq!(
        mounted.state.bytes_written.load(Ordering::SeqCst),
        allowed_cpus().len() as u64 * ((1 << 20) + 100)
    );
    mounted.unmount().unwrap();
}

/// Requests answered from other threads, many at once from every CPU
#[test]
fn replies_from_other_threads() {
    if !uring_usable() {
        return;
    }
    let mounted = Mounted::new(IoUringMode::Required, None).unwrap();
    let clients: Vec<_> = (0..16)
        .map(|_| {
            let slow = mounted.path("slow");
            thread::spawn(move || {
                for _ in 0..50 {
                    assert_eq!(std::fs::metadata(&slow).unwrap().len(), FILE_SIZE);
                }
            })
        })
        .collect();
    for client in clients {
        client.join().unwrap();
    }
    mounted.unmount().unwrap();
}

/// A reply kept by the filesystem and sent while a later request on the same queue is served,
/// by the same thread
#[test]
fn reply_held_over_to_a_later_request() {
    if !uring_usable() {
        return;
    }
    // The held request keeps one entry, and the thread has another for the next request
    let mounted = Mounted::with_queues(IoUringMode::Required, None, 2, 1).unwrap();
    let cpu = allowed_cpus()[0];
    let held = mounted.path("held");
    let waiter = thread::spawn(move || {
        super::pin_to_cpu(cpu);
        std::fs::metadata(held).map(|m| m.len())
    });
    {
        let mut held = mounted.state.held.lock().unwrap();
        while held.is_none() {
            held = mounted.state.held_cond.wait(held).unwrap();
        }
    }
    let file = mounted.path("file");
    on_cpu(cpu, move || std::fs::metadata(file).unwrap());
    assert_eq!(waiter.join().unwrap().unwrap(), FILE_SIZE);
    mounted.unmount().unwrap();
}

/// A filesystem method that blocks holds up no other request from its CPU while the queue has
/// another thread
#[test]
fn blocked_method_holds_up_no_other_request() {
    if !uring_usable() {
        return;
    }
    let mounted = Mounted::with_queues(IoUringMode::Required, None, 2, 2).unwrap();
    let cpu = allowed_cpus()[0];
    let blocked = mounted.path("blocked");
    let blocked = thread::spawn(move || {
        super::pin_to_cpu(cpu);
        std::fs::metadata(blocked).map(|m| m.len())
    });
    {
        let mut gate = mounted.state.gate.lock().unwrap();
        while !gate.0 {
            gate = mounted.state.gate_cond.wait(gate).unwrap();
        }
    }
    let (done, finished) = std::sync::mpsc::channel();
    let file = mounted.path("file");
    thread::spawn(move || {
        super::pin_to_cpu(cpu);
        let _ = done.send(std::fs::metadata(file).map(|m| m.len()));
    });
    let unblocked = finished.recv_timeout(Duration::from_secs(10));
    let mut gate = mounted.state.gate.lock().unwrap();
    gate.1 = true;
    mounted.state.gate_cond.notify_all();
    drop(gate);
    assert_eq!(
        unblocked
            .expect("a request waited behind a blocked one")
            .unwrap(),
        FILE_SIZE
    );
    assert_eq!(blocked.join().unwrap().unwrap(), FILE_SIZE);
    mounted.unmount().unwrap();
}

/// A panic costs the request that caused it, not its queue
#[test]
fn panic_is_answered_with_eio() {
    if !uring_usable() {
        return;
    }
    let mounted = Mounted::new(IoUringMode::Required, None).unwrap();
    let cpu = allowed_cpus()[0];
    let (panic, file) = (mounted.path("panic"), mounted.path("file"));
    on_cpu(cpu, move || {
        let err = std::fs::metadata(panic).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EIO), "{err}");
        assert_eq!(std::fs::metadata(file).unwrap().len(), FILE_SIZE);
    });
    mounted.unmount().unwrap();
}

/// A queue that cannot be registered sends the session back to `/dev/fuse`
#[test]
fn falls_back_to_dev_fuse_when_a_queue_fails() {
    if !uring_usable() {
        return;
    }
    let queues = super::possible_cpus().unwrap();
    let mounted = Mounted::new(IoUringMode::Auto, Some(queues - 1)).unwrap();
    for cpu in allowed_cpus() {
        let file = mounted.path("file");
        on_cpu(cpu, move || {
            assert_eq!(std::fs::metadata(&file).unwrap().len(), FILE_SIZE);
            let data = read_exact_at(&file, 0, 4096);
            assert_eq!(data[100], pattern(100));
        });
    }
    let served_by = mounted.served_by();
    assert!(
        served_by.iter().all(|name| name.starts_with("fuser-0")),
        "{served_by:?}"
    );
    mounted.unmount().unwrap();
}

/// Where io_uring is required, a queue that cannot be registered ends the session
#[test]
fn required_fails_when_a_queue_fails() {
    if !uring_usable() {
        return;
    }
    let mut mounted = Mounted::new(IoUringMode::Required, Some(0)).unwrap();
    let session = mounted.session.take().unwrap();
    let err = session.join().unwrap_err();
    assert!(err.to_string().contains("injected failure"), "{err}");
    unsafe { ManuallyDrop::drop(&mut mounted.tmp) };
}

/// Aborting the connection while a reply is outstanding ends the session, and the late reply
/// goes nowhere
#[test]
fn abort_with_a_reply_outstanding() {
    if !uring_usable() {
        return;
    }
    let mounted = Mounted::new(IoUringMode::Required, None).unwrap();
    let Some(abort) = crate::session::test::fusectl_abort_path(&mounted.mountpoint) else {
        eprintln!("skipping: fusectl not available");
        mounted.unmount().unwrap();
        return;
    };
    let held = mounted.path("held");
    let waiter = thread::spawn(move || std::fs::metadata(held));
    {
        let mut held = mounted.state.held.lock().unwrap();
        while held.is_none() {
            held = mounted.state.held_cond.wait(held).unwrap();
        }
    }
    std::fs::write(abort, "1").unwrap();
    assert!(waiter.join().unwrap().is_err());
    // Dropping the reply sends EIO for it, which nothing takes any more
    drop(mounted.state.held.lock().unwrap().take());
    let mut mounted = mounted;
    mounted.session.take().unwrap().umount_and_join().unwrap();
    // The aborted mount stays in the mount table until detached
    let _ = nix::mount::umount2(&mounted.mountpoint, nix::mount::MntFlags::MNT_DETACH);
    unsafe { ManuallyDrop::drop(&mut mounted.tmp) };
}

/// Where io_uring can be used, `Auto` uses it
#[test]
fn auto_uses_io_uring_where_it_can() {
    if !uring_usable() {
        return;
    }
    let mounted = Mounted::new(IoUringMode::Auto, None).unwrap();
    assert_eq!(
        std::fs::metadata(mounted.path("file")).unwrap().len(),
        FILE_SIZE
    );
    let served_by = mounted.served_by();
    assert!(
        !served_by.is_empty() && served_by.iter().all(|n| n.starts_with("fuser-uring-")),
        "{served_by:?}"
    );
    mounted.unmount().unwrap();
}

/// Many clients on every CPU at once, with every kind of reply
#[test]
fn concurrent_mixed_operations() {
    if !uring_usable() {
        return;
    }
    let mounted = Mounted::new(IoUringMode::Required, None).unwrap();
    let cpus = allowed_cpus();
    let clients: Vec<_> = (0..16)
        .map(|client| {
            let cpu = cpus[client % cpus.len()];
            let (file, slow) = (mounted.path("file"), mounted.path("slow"));
            thread::spawn(move || {
                super::pin_to_cpu(cpu);
                let handle = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&file)
                    .unwrap();
                for i in 0..200u64 {
                    let offset = ((client as u64 * 7 + i) % 8) * (256 << 10);
                    match i % 4 {
                        0 => assert_eq!(std::fs::metadata(&slow).unwrap().len(), FILE_SIZE),
                        1 => {
                            let mut buf = vec![0; 64 << 10];
                            handle.read_exact_at(&mut buf, offset).unwrap();
                            assert!((offset..).zip(&buf).all(|(pos, &b)| b == pattern(pos)));
                        }
                        2 => {
                            let data: Vec<u8> = (offset..offset + 8192).map(pattern).collect();
                            handle.write_all_at(&data, offset).unwrap();
                        }
                        _ => assert_eq!(std::fs::metadata(&file).unwrap().len(), FILE_SIZE),
                    }
                }
            })
        })
        .collect();
    for client in clients {
        client.join().unwrap();
    }
    assert_eq!(mounted.state.corrupt_writes.load(Ordering::SeqCst), 0);
    assert_eq!(
        mounted.state.bytes_written.load(Ordering::SeqCst),
        16 * 50 * 8192
    );
    mounted.unmount().unwrap();
}
