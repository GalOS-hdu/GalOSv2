# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

GalOSv2 (Starry OS) is an experimental monolithic OS kernel in Rust, built on top of the [ArceOS](https://github.com/arceos-org/arceos) unikernel framework. It runs Linux user-space programs by implementing the Linux syscall ABI. The `arceos/` submodule provides hardware abstraction (HAL, memory, tasks, drivers, filesystems, networking), while `kernel/` implements the Linux-compatible kernel layer (syscalls, process model, user address space, file descriptors, signals).

## Build Commands

All commands run from the repository root. The top-level `Makefile` delegates to `make/Makefile`.

```bash
# Prerequisites: musl cross-compiler in PATH, QEMU installed
# Rust toolchain auto-installs via rust-toolchain.toml (nightly-2026-02-25)

# Download root filesystem disk image (required before first run)
make rootfs                        # default: riscv64
make ARCH=loongarch64 rootfs

# Build
make build                         # default: ARCH=riscv64
make ARCH=aarch64 build

# Build and run in QEMU
make run                           # default: riscv64
make ARCH=loongarch64 run
make rv                            # alias: riscv64 run
make la                            # alias: loongarch64 run

# Useful variables
# ARCH=riscv64|loongarch64|aarch64|x86_64  LOG=warn|info|debug|trace
# MODE=release|debug  SMP=1  MEM=1G  GRAPHIC=y|n  INPUT=y|n

# Debugging
make debug                         # build + attach GDB
```

## Formatting and Linting

```bash
cargo fmt --all --check            # CI check
cargo fmt --all                    # auto-format

# Clippy (CI runs all 4 arch targets)
cargo clippy --target riscv64gc-unknown-none-elf -F qemu -- -D warnings -A unused
cargo clippy --target loongarch64-unknown-none-softfloat -F qemu -- -D warnings -A unused
cargo clippy --target aarch64-unknown-none-softfloat -F qemu -- -D warnings -A unused
cargo clippy --target x86_64-unknown-none -F qemu -- -D warnings -A unused
```

## CI Integration Test

```bash
python3 scripts/ci-test.py riscv64   # boots QEMU, waits for shell prompt
```

## Architecture

### Two-Layer Design

- **`arceos/`** (git submodule): Hardware abstraction layer — `axhal` (traps, paging, IRQ), `axmm` (kernel memory), `axtask` (scheduler), `axfs` (ext4 filesystem), `axnet` (network stack), `axdriver` (virtio drivers), `axruntime` (boot sequence).
- **`kernel/`** (`starry-kernel` crate): Linux-compatible kernel built on ArceOS — syscall dispatch, process/thread model, user address space with COW/file-backed/shared memory backends, ELF loader, FD table, pseudo-filesystems (/dev, /proc, /sys, /tmp), signal handling, futex.

### Key Kernel Modules

- **`syscall/`**: Linux syscall dispatch (`handle_syscall`) on `Sysno` enum. Subdirectories: `fs/`, `mm/`, `task/`, `net/`, `signal/`, `sync/`, `io_mpx/`, `ipc/`, `time/`.
- **`mm/`**: User address space (`AddrSpace`), page fault handling, ELF loader. Memory backends in `aspace/backend/`: `Linear`, `Cow`, `File`, `Shared`.
- **`task/`**: `Thread` (per-thread) and `ProcessData` (shared per-process: address space, FD table, signals, resource limits). Uses `scope_local!` for per-process scoped data.
- **`file/`**: `FileLike` trait unifying regular files, sockets, pipes, eventfd, etc. FD table via `FlattenObjects`.
- **`pseudofs/`**: Virtual filesystems — `/dev` (console/N_TTY, null, zero, urandom), `/proc`, `/tmp`, `/sys`.
- **`entry.rs`**: Kernel entry point, init process setup.

### Build Flow

1. `make defconfig` → `axconfig-gen` merges `make/defconfig.toml` + platform config → `.axconfig.toml`
2. `cargo build` with `AX_CONFIG_PATH=.axconfig.toml`, bare-metal target triple
3. `rust-objcopy` produces flat binary, QEMU loads it

### Workspace Structure

The workspace has one member (`kernel/`). The root `Cargo.toml` is the binary crate `starryos` which depends on `axfeat` and `starry-kernel`. The `arceos/` submodule is excluded from the workspace.

## Supported Architectures

| Architecture | Target | Status |
|---|---|---|
| RISC-V 64 | `riscv64gc-unknown-none-elf` | Primary/default |
| LoongArch64 | `loongarch64-unknown-none-softfloat` | Supported |
| AArch64 | `aarch64-unknown-none-softfloat` | Supported |
| x86_64 | `x86_64-unknown-none` | Work in progress |

Architecture-specific code uses `#[cfg(target_arch = "...")]` guards. Per-arch constants are in `kernel/src/config/`.

## Conventions

- Rust edition 2024, `no_std` environment (bare-metal kernel)
- Commit messages follow conventional commits: `feat(scope):`, `fix(scope):`, `chore:`, etc.
- rust-analyzer is configured via `.vscode/settings.json` for riscv64 with `features = ["qemu"]`
- The disk image is **not** reset between runs; switch architectures with `make ARCH=<arch> rootfs` before running
