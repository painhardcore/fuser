# FUSE (Filesystem in Userspace) for Rust

![CI](https://github.com/cberner/fuser/actions/workflows/ci.yml/badge.svg)
[![Crates.io](https://img.shields.io/crates/v/fuser.svg)](https://crates.io/crates/fuser)
[![Documentation](https://docs.rs/fuser/badge.svg)](https://docs.rs/fuser)
[![MIT License](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/cberner/fuser/blob/master/LICENSE.md)
[![dependency status](https://deps.rs/repo/github/cberner/fuser/status.svg)](https://deps.rs/repo/github/cberner/fuser)

## About

**FUSE-Rust** is a [Rust] library crate for easy implementation of [FUSE filesystems][FUSE for Linux] in userspace.

FUSE-Rust does not just provide bindings, it is a rewrite of the original FUSE C library to fully take advantage of Rust's architecture.

This library was originally forked from the [`fuse` crate](https://github.com/zargony/fuse-rs) with the intention
of continuing development. In particular adding features from ABIs after 7.19

## Use of AI

Version 0.18.0 is the last version that was primarily developed without coding agents.

Future releases will be developed primarily by a coding agent, as I believe this will lead to higher quality code,
and faster feature development.
All the changes will receive at least a cursory review from a human, and a full review from a coding agent.

## Documentation

[FUSE-Rust reference][Documentation]

## Details

A working FUSE filesystem consists of three parts:

1. The **kernel driver** that registers as a filesystem and forwards operations into a communication channel to a userspace process that handles them.
1. The **userspace library** (libfuse) that helps the userspace process to establish and run communication with the kernel driver.
1. The **userspace implementation** that actually processes the filesystem operations.

The kernel driver is provided by the FUSE project, the userspace implementation needs to be provided by the developer. FUSE-Rust provides a replacement for the libfuse userspace library between these two. This way, a developer can fully take advantage of the Rust type interface and runtime features when building a FUSE filesystem in Rust.

Except for a single setup (mount) function call and a final teardown (umount) function call to libfuse, everything runs in Rust, and on Linux these calls to libfuse are optional. They can be removed by building without the "libfuse" feature flag.

## Dependencies

FUSE must be installed to build or run programs that use FUSE-Rust (i.e. kernel driver and libraries. Some platforms may also require userland utils like `fusermount`). A default installation of FUSE is usually sufficient.

To build FUSE-Rust or any program that depends on it, `pkg-config` needs to be installed as well.

### Linux

[FUSE for Linux] is available in most Linux distributions and usually called `fuse` or `fuse3` (this crate is compatible with both). To install on a Debian based system:

```sh
sudo apt-get install fuse3 libfuse3-dev
```

Install on CentOS:

```sh
sudo yum install fuse
```

To build, FUSE libraries and headers are required. The package is usually called `libfuse-dev` or `fuse-devel`. Also `pkg-config` is required for locating libraries and headers.

```sh
sudo apt-get install libfuse-dev pkg-config
```

```sh
sudo yum install fuse-devel pkgconfig
```

### macOS (untested)

Install [FUSE for macOS], which can be obtained from their website or installed using the Homebrew or Nix package managers. macOS version 10.9 or later is required. If you are using a Mac with Apple Silicon, you must also [enable support for third party kernel extensions][enable kext].


#### To install using Homebrew

```sh
brew install macfuse pkgconf
```

#### To install using Nix

``` sh
nix-env -iA nixos.macfuse-stubs nixos.pkg-config
```

When using `nix` it is required that you specify `PKG_CONFIG_PATH` environment variable to point at where `macfuse` is installed:

``` sh
export PKG_CONFIG_PATH=${HOME}/.nix-profile/lib/pkgconfig
```

### FreeBSD

Install packages `fusefs-libs` and `pkgconf`.

```sh
pkg install fusefs-libs pkgconf
```

## Usage

```sh
cargo add fuser
```

or put this in your `Cargo.toml`:

```toml
[dependencies]
fuser = "0.15"
```

To create a new filesystem, implement the trait `fuser::Filesystem`. See the [documentation] for details or the `examples` directory for some basic examples.

## FUSE over io_uring

On Linux 6.14 and later, a session can exchange requests with the kernel over io_uring instead
of `read(2)` and `writev(2)` on `/dev/fuse`. The kernel keeps one queue per CPU, the session
serves each with threads pinned to that CPU, and replying to a request and fetching the next
take one system call. The `Filesystem` trait is unchanged.

```rust
let mut config = fuser::Config::default();
// Falls back to /dev/fuse where io_uring cannot be used; IoUringMode::Required fails instead
config.io_uring.mode = fuser::IoUringMode::Auto;
```

The kernel only offers it once the `fuse` module allows it:

```sh
echo Y | sudo tee /sys/module/fuse/parameters/enable_uring
```

and the process must be allowed to use io_uring, which Docker's default seccomp profile and the
`kernel.io_uring_disabled` sysctl can prevent. With `IoUringMode::Auto` any of these simply
leaves the session on `/dev/fuse`, saying why in the log.

Things to know:

* The filesystem is called from every queue's threads at once, so it must cope with concurrent
  calls, as it already must with `Config::n_threads`.
* Each queue is served by `threads_per_queue` threads (4 by default), each holding one of the
  queue's `queue_depth` entries. A thread busy in a slow method then holds up nothing else from
  its CPU. That matters for requests the caller does not wait for: `close(2)` returns before
  `release()` is answered, and the caller's next request would otherwise queue behind it.
* With `pin_threads` (the default) a request is served on the CPU that issued it. For a single
  caller most of the gain below comes from that placement rather than from the transport: in
  the same VM, unpinned io_uring serves one caller about as fast as `/dev/fuse`. On hardware
  whose clock follows load, the placement can instead show up as a higher clock, and the
  filesystem's work competes with the caller for its CPU. What placement does not explain is
  the gain with many callers, where `/dev/fuse` funnels every request through one contended
  queue; a filesystem that serializes its requests behind one lock cannot use that.
* Each queue entry reserves a buffer of `max_write` (16 MiB unless lowered with
  `KernelConfig::set_max_write()`), which only takes memory as large requests pass through it.
* `ReplyData::fill()` writes a read's data straight into the buffer the kernel copies it from,
  where `ReplyData::data()` copies it there first.
* A panic in a filesystem method called from a queue answers that request with `EIO` and the
  queue carries on.

`make bench` compares the transports (`examples/bench.rs`). On a 4-CPU VM running Linux 6.18,
with every request reaching the filesystem (direct I/O, no attribute caching), reads served from
`/dev/shm` with `ReplyData::fill()`, writes discarded, files created and listed in memory and
4 clients in the parallel rows:

| workload | `/dev/fuse`, 1 thread | `/dev/fuse`, 4 threads | io_uring |
|---|---|---|---|
| stat, 1 client | 17.0k ops/s | 16.2k ops/s | 39.1k ops/s |
| stat, 4 clients | 49.6k ops/s | 68.5k ops/s | 387k ops/s |
| read 4k, 1 client | 136 MiB/s | 113 MiB/s | 282 MiB/s |
| read 4k, 4 clients | 342 MiB/s | 463 MiB/s | 2759 MiB/s |
| write 4k, 1 client | 133 MiB/s | 127 MiB/s | 583 MiB/s |
| read 128k, 1 client | 2633 MiB/s | 1520 MiB/s | 3203 MiB/s |
| write 128k, 1 client | 2795 MiB/s | 2502 MiB/s | 3877 MiB/s |
| read 1M, 1 client | 3417 MiB/s | 2377 MiB/s | 2052 MiB/s |
| read 1M, 4 clients | 4080 MiB/s | 14612 MiB/s | 17267 MiB/s |
| write 1M, 1 client | 3985 MiB/s | 2571 MiB/s | 3227 MiB/s |
| create 4k file, 1 client | 6.4k files/s | 5.7k files/s | 18.1k files/s |
| create 4k file, 4 clients | 18.9k files/s | 22.9k files/s | 70.3k files/s |
| list 1000 entries, 1 client | 3.5k lists/s | 2.5k lists/s | 6.1k lists/s |

A single client streaming large requests loses, since serving them on its own CPU costs more
than it saves. `--slow release --delay-us 100` makes `release()` sleep, as one doing I/O would:
with one thread per queue, creating 4k files then drops to 3.5k files/s against 5.6k over
`/dev/fuse` with 4 threads; with the default four it reaches 11.3k.

## To Do

Most features of libfuse up to 3.10.3 are implemented. Feel free to contribute. See the [list of issues][issues] on GitHub and search the source files for comments containing "`TODO`" or "`FIXME`" to see what's still missing.

## Compatibility

Developed and tested on Linux. Tested under [Linux][FUSE for Linux] and [FreeBSD][FUSE for FreeBSD] using stable [Rust] (see CI for details).

## License

Licensed under [MIT License](LICENSE.md), except for those files in `examples/` that explicitly contain a different license.

## Contribution

**Pull requests are no longer being accepted. Please file an issue, or fork the project instead.**

Fork, hack, ~~submit pull request~~. Make sure to make it useful for the target audience, keep the project's philosophy and Rust coding standards in mind. For larger or essential changes, you may want to open an issue for discussion first. Also remember to update the [Changelog] if your changes are relevant to the users.

[Rust]: https://rust-lang.org
[Homebrew]: https://brew.sh
[Changelog]: https://keepachangelog.com/en/1.0.0/

[FUSE-Rust]: https://github.com/cberner/fuser
[issues]: https://github.com/cberner/fuser/issues
[Documentation]: https://docs.rs/fuser

[FUSE for Linux]: https://github.com/libfuse/libfuse/
[FUSE for macOS]: https://macfuse.github.io
[enable kext]: https://github.com/macfuse/macfuse/wiki/Getting-Started#enabling-support-for-third-party-kernel-extensions-apple-silicon-macs
[FUSE for FreeBSD]: https://wiki.freebsd.org/FUSEFS
