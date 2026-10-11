//! Compares the `/dev/fuse` and io_uring transports.
//!
//! Mounts a filesystem that sends every request to userspace (no attribute or entry caching,
//! direct I/O), serves reads from a file in `/dev/shm` and discards writes, and measures what
//! clients see through it.
//!
//! ```text
//! cargo run --release --example bench -- --seconds 5
//! ```
//!
//! io_uring needs Linux 6.14 or later with `echo Y > /sys/module/fuse/parameters/enable_uring`.

use std::ffi::OsStr;
use std::fs::File;
use std::fs::OpenOptions;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Barrier;
use std::sync::atomic::AtomicBool;
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
use fuser::ReplyAttr;
use fuser::ReplyData;
use fuser::ReplyEntry;
use fuser::ReplyOpen;
use fuser::ReplyWrite;
use fuser::Request;
use fuser::Session;
use fuser::WriteFlags;

const FILE: INodeNo = INodeNo(2);
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
    #[clap(long, default_value_t = 8)]
    queue_depth: u16,

    /// Only run measurements whose name contains this
    #[clap(long)]
    filter: Option<String>,

    /// Answer reads with `ReplyData::data`, which copies the data into the reply, instead of
    /// `ReplyData::fill`, which reads it straight into the reply buffer
    #[clap(long)]
    copy_reads: bool,
}

/// Reads are served from a file of this size, as a filesystem passing them through would
const BACKING_SIZE: u64 = 64 << 20;

struct BenchFs {
    backing: File,
    /// Answer reads with `ReplyData::data` rather than `ReplyData::fill`
    copy_reads: bool,
}

fn attr(ino: INodeNo) -> FileAttr {
    let (kind, size) = if ino == INodeNo::ROOT {
        (FileType::Directory, 0)
    } else {
        (FileType::RegularFile, FILE_SIZE)
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

impl Filesystem for BenchFs {
    fn lookup(&self, _req: &Request, _parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        if name == "file" {
            reply.entry(&Duration::ZERO, &attr(FILE), Generation(0));
        } else {
            reply.error(Errno::ENOENT);
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        reply.attr(&Duration::ZERO, &attr(ino));
    }

    fn open(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, _: bool, reply: ReplyOpen) {
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
        Op::Stat => 0,
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
    copy_reads: bool,
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
        }
    }
    // In memory, so that what is measured is the transport rather than a disk
    let backing = tempfile::tempfile_in("/dev/shm")?;
    backing.set_len(BACKING_SIZE)?;
    let fs = BenchFs {
        backing,
        copy_reads,
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
    let mut results: Vec<Vec<Option<Measurement>>> = vec![];
    for transport in transports {
        let dir = tempfile::tempdir().unwrap();
        let session = match mount(transport, dir.path(), args.copy_reads) {
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
        transports.map(|t| t.name()).join(" | ")
    );
    println!("|---|{}", "---|".repeat(transports.len()));
    for (i, workload) in workloads.iter().enumerate() {
        let cells: Vec<String> = results
            .iter()
            .map(|column| match &column[i] {
                None => "n/a".to_string(),
                Some(m) if matches!(workload.op, Op::Stat) || m.mib_per_sec < 1.0 => {
                    format!("{:.0} ops/s (p50 {:.1}us)", m.ops_per_sec, m.p50_us)
                }
                Some(m) => format!("{:.0} MiB/s (p50 {:.1}us)", m.mib_per_sec, m.p50_us),
            })
            .collect();
        println!("| {} | {} |", workload.name, cells.join(" | "));
    }
}
