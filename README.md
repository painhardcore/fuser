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
serves each from a thread pinned to that CPU, and replying to a request and fetching the next
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

* The filesystem is called from every queue's thread at once, so it must cope with concurrent
  calls, as it already must with `Config::n_threads`. A method that blocks for long holds up
  the other requests issued on its CPU; replying from another thread avoids that.
* Each queue entry reserves a buffer of `max_write` (16 MiB unless lowered with
  `KernelConfig::set_max_write()`), which only takes memory as large requests pass through it.
* `ReplyData::fill()` writes a read's data straight into the buffer the kernel copies it from,
  where `ReplyData::data()` copies it there first.
* A panic in a filesystem method called from a queue answers that request with `EIO` and the
  queue carries on.

`make bench` compares the transports (`examples/bench.rs`). On a 4-CPU VM running Linux 6.18,
with every request reaching the filesystem (direct I/O, no attribute caching), reads served from
`/dev/shm` with `ReplyData::fill()` and writes discarded, 4 clients in the parallel rows:

| workload | `/dev/fuse`, 1 thread | `/dev/fuse`, 4 threads | io_uring |
|---|---|---|---|
| stat, 1 client | 17.5k ops/s | 16.4k ops/s | 40.2k ops/s |
| stat, 4 clients | 48.6k ops/s | 66.6k ops/s | 368k ops/s |
| read 4k, 1 client | 133 MiB/s | 127 MiB/s | 287 MiB/s |
| read 4k, 4 clients | 363 MiB/s | 500 MiB/s | 2934 MiB/s |
| write 4k, 1 client | 130 MiB/s | 139 MiB/s | 471 MiB/s |
| read 128k, 1 client | 2473 MiB/s | 2140 MiB/s | 3573 MiB/s |
| write 128k, 1 client | 2706 MiB/s | 2628 MiB/s | 4525 MiB/s |
| read 1M, 1 client | 3549 MiB/s | 1962 MiB/s | 2110 MiB/s |
| read 1M, 4 clients | 4339 MiB/s | 13520 MiB/s | 15725 MiB/s |
| write 1M, 1 client | 3516 MiB/s | 2883 MiB/s | 2933 MiB/s |

Median latency of a 4k read drops from about 27us to 5-10us. The one case that loses is a
single client streaming large requests, where serving them on the client's own CPU costs more
than it saves.

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
