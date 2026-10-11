//! Compares the `/dev/fuse` and io_uring transports.
//!
//! Mounts a filesystem that sends every request to userspace (no attribute or entry caching,
//! direct I/O), serves reads from a file in `/dev/shm` and discards writes, and measures what
//! clients see through it. Files are created and listed in memory, each client in a
//! directory of its own.
//!
//! ```text
//! cargo run --release --example bench -- --seconds 5
//! ```
//!
//! io_uring needs Linux 6.14 or later with `echo Y > /sys/module/fuse/parameters/enable_uring`.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs::File;
use std::fs::OpenOptions;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Barrier;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

use clap::Parser;
use fuser::Config;
use fuser::Errno;
use fuser::FileAttr;
use fuser::FileHandle;
use fuser::FileType;
use fuser::Filesystem;
use fuser::FopenFlags;
use fuser::Generation;
use fuser::INodeNo;
use fuser::IoUringMode;
use fuser::LockOwner;
use fuser::OpenFlags;
use fuser::Owner;
use fuser::ReplyAttr;
use fuser::ReplyCreate;
use fuser::ReplyData;
use fuser::ReplyDirectory;
use fuser::ReplyEmpty;
use fuser::ReplyEntry;
use fuser::ReplyOpen;
use fuser::ReplyWrite;
use fuser::Request;
use fuser::Session;
use fuser::WriteFlags;

const FILE: INodeNo = INodeNo(2);
/// Directories, one for each client, are numbered from here
const FIRST_DIR: u64 = 1000;
const MAX_CLIENTS: usize = 1024;
/// Holds `LISTED_ENTRIES` files for listing
const LISTED: INodeNo = INodeNo(FIRST_DIR + MAX_CLIENTS as u64);
const LISTED_ENTRIES: usize = 1000;
/// Created files are numbered from here
const FIRST_CREATED: u64 = 1 << 20;
/// Names created files uniquely across workloads
static NEXT_NAME: AtomicU64 = AtomicU64::new(0);
const FILE_SIZE: u64 = 1 << 40;
const MAX_IO: usize = 1 << 20;

#[derive(Parser)]
struct Args {
    /// How long each measurement runs
    #[clap(long, default_value_t = 3.0)]
    seconds: f64,

    /// Client threads for the parallel measurements; defaults to the number of CPUs
    #[clap(long)]
    clients: Option<usize>,

    /// Requests each io_uring queue holds at once
    #[clap(long, default_value_t = 4)]
    queue_depth: u16,

    /// Threads serving each io_uring queue
    #[clap(long, default_value_t = 4)]
    threads_per_queue: u16,

    /// Leave io_uring queue threads unpinned
    #[clap(long)]
    no_pin: bool,

    /// Only run measurements whose name contains this
    #[clap(long)]
    filter: Option<String>,

    /// Answer reads with `ReplyData::data`, which copies the data into the reply, instead of
    /// `ReplyData::fill`, which reads it straight into the reply buffer
    #[clap(long)]
    copy_reads: bool,

    /// Operations that wait `--delay-us` before answering, as one doing I/O would: any of
    /// create, write, flush, release
    #[clap(long, value_delimiter = ',')]
    slow: Vec<String>,

    /// How long a slow operation waits
    #[clap(long, default_value_t = 100)]
    delay_us: u64,

    /// Slow operations keep the CPU busy for `--delay-us` rather than sleeping
    #[clap(long)]
    busy: bool,

    /// Only measure transports whose name contains this, such as "io_uring"
    #[clap(long)]
    only: Option<String>,
}

/// Reads are served from a file of this size, as a filesystem passing them through would
const BACKING_SIZE: u64 = 64 << 20;

struct BenchFs {
    /// The files in each directory, by name
    dirs: Vec<Mutex<BTreeMap<OsString, u64>>>,
    next_ino: AtomicU64,
    backing: File,
    /// Answer reads with `ReplyData::data` rather than `ReplyData::fill`
    copy_reads: bool,
    slow: Vec<String>,
    delay: Duration,
    busy: bool,
}

fn is_dir(ino: INodeNo) -> bool {
    ino == INodeNo::ROOT || (FIRST_DIR..=LISTED.0).contains(&ino.0)
}

fn attr(ino: INodeNo) -> FileAttr {
    let (kind, size) = if is_dir(ino) {
        (FileType::Directory, 0)
    } else if ino == FILE {
        (FileType::RegularFile, FILE_SIZE)
    } else {
        (FileType::RegularFile, 0)
    };
    FileAttr {
        ino,
        size,
        blocks: size / 512,
        atime: SystemTime::UNIX_EPOCH,
        mtime: SystemTime::UNIX_EPOCH,
        ctime: SystemTime::UNIX_EPOCH,
        crtime: SystemTime::UNIX_EPOCH,
        kind,
        perm: 0o777,
        nlink: 1,
        uid: 0,
        gid: 0,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

impl BenchFs {
    fn maybe_wait(&self, op: &str) {
        if self.slow.iter().any(|slow| slow == op) {
            if self.busy {
                let began = Instant::now();
                while began.elapsed() < self.delay {
                    std::hint::spin_loop();
                }
            } else {
                thread::sleep(self.delay);
            }
        }
    }

    fn dir(&self, ino: INodeNo) -> Option<&Mutex<BTreeMap<OsString, u64>>> {
        self.dirs.get(ino.0.checked_sub(FIRST_DIR)? as usize)
    }

    fn find(&self, parent: INodeNo, name: &OsStr) -> Option<INodeNo> {
        if parent != INodeNo::ROOT {
            return self
                .dir(parent)?
                .lock()
                .unwrap()
                .get(name)
                .copied()
                .map(INodeNo);
        }
        let name = name.to_str()?;
        if name == "file" {
            return Some(FILE);
        }
        if name == "listed" {
            return Some(LISTED);
        }
        let client: u64 = name.strip_prefix('d')?.parse().ok()?;
        (client < MAX_CLIENTS as u64).then_some(INodeNo(FIRST_DIR + client))
    }
}

impl Filesystem for BenchFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        match self.find(parent, name) {
            Some(ino) => reply.entry(&Duration::ZERO, &attr(ino), Generation(0)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        _kill_suid_gid: bool,
        _owner: Owner,
        reply: ReplyCreate,
    ) {
        self.maybe_wait("create");
        let Some(dir) = self.dir(parent) else {
            return reply.error(Errno::EACCES);
        };
        let ino = self.next_ino.fetch_add(1, Ordering::Relaxed);
        if dir.lock().unwrap().insert(name.to_owned(), ino).is_some() {
            return reply.error(Errno::EEXIST);
        }
        reply.created(
            &Duration::ZERO,
            &attr(INodeNo(ino)),
            Generation(0),
            FileHandle(0),
            FopenFlags::empty(),
        );
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match self.dir(parent).map(|dir| dir.lock().unwrap().remove(name)) {
            Some(Some(_)) => reply.ok(),
            _ => reply.error(Errno::ENOENT),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        self.maybe_wait("flush");
        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.maybe_wait("release");
        reply.ok();
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let Some(dir) = self.dir(ino) else {
            return reply.error(Errno::ENOTDIR);
        };
        let dir = dir.lock().unwrap();
        for (i, (name, &ino)) in dir.iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(ino), i as u64 + 1, FileType::RegularFile, name) {
                break;
            }
        }
        reply.ok();
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        reply.attr(&Duration::ZERO, &attr(ino));
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, _: bool, reply: ReplyOpen) {
        // Created files go through the page cache, as files usually do
        let flags = if ino == FILE {
            FopenFlags::FOPEN_DIRECT_IO
        } else {
            FopenFlags::empty()
        };
        reply.opened(FileHandle(0), flags);
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
        let size = (size as usize).min(MAX_IO);
        let offset = offset % (BACKING_SIZE - MAX_IO as u64);
        let read = |buf: &mut [u8]| {
            self.backing
                .read_at(buf, offset)
                .map_err(|err| Errno::from_i32(err.raw_os_error().unwrap_or(libc::EIO)))
        };
        if self.copy_reads {
            let mut buf = vec![0; size];
            match read(&mut buf) {
                Ok(n) => reply.data(&buf[..n]),
                Err(err) => reply.error(err),
            }
        } else {
            reply.fill(size, read);
        }
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        self.maybe_wait("write");
        reply.written(data.len() as u32);
    }
}

#[derive(Clone, Copy)]
enum Transport {
    DevFuse { threads: usize },
    IoUring { depth: u16 },
}

impl Transport {
    fn name(self) -> String {
        match self {
            Transport::DevFuse { threads: 1 } => "/dev/fuse, 1 thread".to_string(),
            Transport::DevFuse { threads } => format!("/dev/fuse, {threads} threads"),
            Transport::IoUring { .. } => "io_uring".to_string(),
        }
    }
}

#[derive(Clone, Copy)]
enum Op {
    Stat,
    Read(usize),
    Write(usize),
    /// Create a file, write this much and close it
    Create(usize),
    /// List a directory of `LISTED_ENTRIES` files
    List,
}

struct Workload {
    name: String,
    op: Op,
    clients: usize,
}

struct Measurement {
    ops_per_sec: f64,
    mib_per_sec: f64,
    /// Median and 99th percentile, sampled from one client
    p50_us: f64,
    p99_us: f64,
}

fn run(mountpoint: &Path, workload: &Workload, seconds: f64) -> Measurement {
    let file = mountpoint.join("file");
    let stop = Arc::new(AtomicBool::new(false));
    let start = Arc::new(Barrier::new(workload.clients + 1));
    let clients: Vec<_> = (0..workload.clients)
        .map(|client| {
            let (file, stop, start, op) = (file.clone(), stop.clone(), start.clone(), workload.op);
            thread::spawn(move || client_loop(&file, client, op, &stop, &start))
        })
        .collect();
    start.wait();
    let began = Instant::now();
    thread::sleep(Duration::from_secs_f64(seconds));
    stop.store(true, Ordering::SeqCst);
    let mut ops = 0u64;
    let mut latencies = vec![];
    for (i, client) in clients.into_iter().enumerate() {
        let (n, lat) = client.join().unwrap();
        ops += n;
        if i == 0 {
            latencies = lat;
        }
    }
    let elapsed = began.elapsed().as_secs_f64();
    latencies.sort_unstable();
    let pct = |p: f64| {
        latencies
            .get(((latencies.len() as f64 * p) as usize).min(latencies.len().saturating_sub(1)))
            .map_or(0.0, |d: &Duration| d.as_secs_f64() * 1e6)
    };
    let bytes = match workload.op {
        Op::Stat | Op::Create(_) | Op::List => 0,
        Op::Read(n) | Op::Write(n) => n,
    };
    Measurement {
        ops_per_sec: ops as f64 / elapsed,
        mib_per_sec: ops as f64 * bytes as f64 / elapsed / (1 << 20) as f64,
        p50_us: pct(0.5),
        p99_us: pct(0.99),
    }
}

fn client_loop(
    file: &PathBuf,
    client: usize,
    op: Op,
    stop: &AtomicBool,
    start: &Barrier,
) -> (u64, Vec<Duration>) {
    let handle = OpenOptions::new()
        .read(true)
        .write(true)
        .open(file)
        .unwrap();
    let mut buf = vec![0xa5u8; MAX_IO];
    let mut latencies = Vec::with_capacity(1 << 20);
    let mut ops = 0u64;
    // Each client works on its own region, as independent clients would
    let mut offset = client as u64 * (1 << 30);
    let mountpoint = file.parent().unwrap();
    let dir = mountpoint.join(format!("d{client}"));
    let listed = mountpoint.join("listed");
    start.wait();
    while !stop.load(Ordering::Relaxed) {
        let began = Instant::now();
        match op {
            Op::Stat => {
                std::fs::metadata(file).unwrap();
            }
            Op::Read(n) => {
                handle.read_exact_at(&mut buf[..n], offset).unwrap();
                offset += n as u64;
            }
            Op::Write(n) => {
                handle.write_all_at(&buf[..n], offset).unwrap();
                offset += n as u64;
            }
            Op::Create(n) => {
                let mut created = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(dir.join(format!("{}", NEXT_NAME.fetch_add(1, Ordering::Relaxed))))
                    .unwrap();
                std::io::Write::write_all(&mut created, &buf[..n]).unwrap();
            }
            Op::List => {
                assert_eq!(std::fs::read_dir(&listed).unwrap().count(), LISTED_ENTRIES);
            }
        }
        if latencies.len() < latencies.capacity() {
            latencies.push(began.elapsed());
        }
        ops += 1;
    }
    drop::<File>(handle);
    (ops, latencies)
}

fn mount(
    transport: Transport,
    mountpoint: &Path,
    args: &Args,
) -> std::io::Result<fuser::BackgroundSession> {
    let mut config = Config::default();
    match transport {
        Transport::DevFuse { threads } => {
            config.n_threads = Some(threads);
            config.clone_fd = threads > 1;
        }
        Transport::IoUring { depth } => {
            config.io_uring.mode = IoUringMode::Required;
            config.io_uring.queue_depth = depth;
            config.io_uring.threads_per_queue = args.threads_per_queue;
            config.io_uring.pin_threads = !args.no_pin;
        }
    }
    // In memory, so that what is measured is the transport rather than a disk
    let backing = tempfile::tempfile_in("/dev/shm")?;
    backing.set_len(BACKING_SIZE)?;
    let mut dirs: Vec<_> = (0..=MAX_CLIENTS)
        .map(|_| Mutex::new(BTreeMap::new()))
        .collect();
    let listed = dirs[MAX_CLIENTS].get_mut().unwrap();
    for i in 0..LISTED_ENTRIES {
        listed.insert(format!("file{i}").into(), FIRST_CREATED + i as u64);
    }
    let fs = BenchFs {
        dirs,
        next_ino: AtomicU64::new(FIRST_CREATED + LISTED_ENTRIES as u64),
        backing,
        copy_reads: args.copy_reads,
        slow: args.slow.clone(),
        delay: Duration::from_micros(args.delay_us),
        busy: args.busy,
    };
    Session::new(fs, mountpoint, &config)?.spawn()
}

fn main() {
    let args = Args::parse();
    env_logger::init();
    let cpus = thread::available_parallelism().map_or(1, |n| n.get());
    let clients = args.clients.unwrap_or(cpus);
    let workloads: Vec<Workload> = [
        ("stat", Op::Stat, 1),
        ("stat", Op::Stat, clients),
        ("read 4k", Op::Read(4096), 1),
        ("read 4k", Op::Read(4096), clients),
        ("write 4k", Op::Write(4096), 1),
        ("write 4k", Op::Write(4096), clients),
        ("read 128k", Op::Read(128 << 10), 1),
        ("write 128k", Op::Write(128 << 10), 1),
        ("read 1M", Op::Read(1 << 20), 1),
        ("read 1M", Op::Read(1 << 20), clients),
        ("write 1M", Op::Write(1 << 20), 1),
        ("write 1M", Op::Write(1 << 20), clients),
        ("create 0", Op::Create(0), 1),
        ("create 0", Op::Create(0), clients),
        ("create 4k", Op::Create(4096), 1),
        ("create 4k", Op::Create(4096), clients),
        ("list 1000", Op::List, 1),
        ("list 1000", Op::List, clients),
    ]
    .into_iter()
    .map(|(name, op, clients)| Workload {
        name: format!(
            "{name}, {clients} client{}",
            if clients == 1 { "" } else { "s" }
        ),
        op,
        clients,
    })
    .filter(|w| {
        args.filter
            .as_ref()
            .is_none_or(|f| w.name.contains(f.as_str()))
    })
    .collect();

    let transports = [
        Transport::DevFuse { threads: 1 },
        Transport::DevFuse { threads: cpus },
        Transport::IoUring {
            depth: args.queue_depth,
        },
    ];
    let transports: Vec<Transport> = transports
        .into_iter()
        .filter(|t| {
            args.only
                .as_ref()
                .is_none_or(|o| t.name().contains(o.as_str()))
        })
        .collect();
    let mut results: Vec<Vec<Option<Measurement>>> = vec![];
    for &transport in &transports {
        let dir = tempfile::tempdir().unwrap();
        let session = match mount(transport, dir.path(), &args) {
            Ok(session) => session,
            Err(err) => {
                eprintln!("{}: cannot mount: {err}", transport.name());
                results.push(workloads.iter().map(|_| None).collect());
                continue;
            }
        };
        let mut column = vec![];
        for workload in &workloads {
            let m = run(dir.path(), workload, args.seconds);
            eprintln!(
                "{:<22} {:<20} {:>10.0} ops/s {:>8.0} MiB/s  p50 {:>7.1}us  p99 {:>7.1}us",
                transport.name(),
                workload.name,
                m.ops_per_sec,
                m.mib_per_sec,
                m.p50_us,
                m.p99_us
            );
            column.push(Some(m));
        }
        session.umount_and_join().unwrap();
        results.push(column);
    }

    println!(
        "| workload | {} |",
        transports
            .iter()
            .map(|t| t.name())
            .collect::<Vec<_>>()
            .join(" | ")
    );
    println!("|---|{}", "---|".repeat(transports.len()));
    for (i, workload) in workloads.iter().enumerate() {
        let cells: Vec<String> = results
            .iter()
            .map(|column| match &column[i] {
                None => "n/a".to_string(),
                Some(m) if m.mib_per_sec < 1.0 => {
                    format!("{:.0} ops/s (p50 {:.1}us)", m.ops_per_sec, m.p50_us)
                }
                Some(m) => format!("{:.0} MiB/s (p50 {:.1}us)", m.mib_per_sec, m.p50_us),
            })
            .collect();
        println!("| {} | {} |", workload.name, cells.join(" | "));
    }
}
