#!/usr/bin/env python3
"""
GalOS Benchmark Runner

Automates: build bench -> inject into rootfs -> build kernel -> boot QEMU -> collect results.

Usage:
    python3 run_bench.py [--arch ARCH] [--timeout SECS] [BENCH_NAMES...]
"""

import argparse
import datetime
import os
import re
import shutil
import socket
import subprocess
import sys
import threading
import time

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
REPO_ROOT = os.path.abspath(os.path.join(SCRIPT_DIR, "../.."))


def make_env(cwd):
    """Return env with PWD set correctly for the given cwd."""
    env = os.environ.copy()
    env["PWD"] = os.path.abspath(cwd)
    return env


def build_bench(arch):
    print(f"[*] Building benchmark for {arch}...")
    ret = subprocess.run(["make", f"ARCH={arch}", "clean", "all"],
                         cwd=SCRIPT_DIR, env=make_env(SCRIPT_DIR))
    if ret.returncode != 0:
        sys.exit(1)
    print("[+] Bench build OK")


def inject_into_rootfs(arch):
    disk = os.path.join(REPO_ROOT, "make", "disk.img")
    bench = os.path.join(SCRIPT_DIR, "bench")

    if not os.path.exists(disk):
        print(f"[*] disk.img not found, running 'make rootfs ARCH={arch}'...")
        subprocess.run(["make", f"ARCH={arch}", "rootfs"],
                       cwd=REPO_ROOT, env=make_env(REPO_ROOT), check=True)

    if shutil.which("e2cp"):
        subprocess.run(["e2cp", bench, f"{disk}:/usr/bin/bench"], check=True)
    elif shutil.which("debugfs"):
        cmd = f"rm /usr/bin/bench\nwrite {bench} /usr/bin/bench\nset_inode_field /usr/bin/bench mode 0100755\n"
        subprocess.run(["debugfs", "-w", disk], input=cmd.encode(),
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    else:
        print("[!] Neither e2cp nor debugfs found.")
        sys.exit(1)
    print("[+] Injected into disk.img")


def build_kernel(arch):
    print(f"[*] Building kernel for {arch}...")
    ret = subprocess.run(["make", f"ARCH={arch}", "build"],
                         cwd=REPO_ROOT, env=make_env(REPO_ROOT))
    if ret.returncode != 0:
        sys.exit(1)
    print("[+] Kernel build OK")


def run_qemu(arch, timeout, bench_args):
    cmd_str = "/usr/bin/bench"
    if bench_args:
        cmd_str += " " + " ".join(bench_args)

    print(f"[*] Booting QEMU ({arch}), running: {cmd_str}")

    qemu_proc = subprocess.Popen(
        ["make", f"ARCH={arch}", "ACCEL=n", "justrun",
         "QEMU_ARGS=-monitor none -serial tcp::4444,server=on"],
        cwd=REPO_ROOT,
        env=make_env(REPO_ROOT),
        stderr=subprocess.PIPE,
        stdout=subprocess.DEVNULL,
        text=True,
    )

    ready = threading.Event()

    def stderr_reader():
        for line in qemu_proc.stderr:
            if "QEMU waiting for connection" in line:
                ready.set()
        ready.set()

    t = threading.Thread(target=stderr_reader, daemon=True)
    t.start()

    try:
        if not ready.wait(timeout=15):
            raise RuntimeError("QEMU did not start in time")
        if qemu_proc.poll() is not None:
            raise RuntimeError("QEMU exited prematurely")

        print("[*] QEMU started, connecting to serial...")
        s = socket.create_connection(("localhost", 4444), timeout=10)
        s.settimeout(2.0)
        buf = ""
        prompt = "starry:~#"
        sent_bench = False
        sent_exit = False
        start = datetime.datetime.now()

        results = []
        in_bench = False

        while True:
            try:
                data = s.recv(4096).decode("utf-8", errors="ignore")
            except socket.timeout:
                elapsed = (datetime.datetime.now() - start).total_seconds()
                if elapsed > timeout:
                    raise RuntimeError(f"Timeout after {timeout}s")
                continue
            except (ConnectionError, OSError):
                break
            if not data:
                break

            # Real-time output of serial data
            sys.stdout.write(data)
            sys.stdout.flush()

            buf += data

            if prompt in buf and not sent_bench:
                time.sleep(0.3)
                s.sendall(f"{cmd_str}\r\n".encode())
                sent_bench = True
                buf = ""

            if sent_bench:
                for line in data.split("\n"):
                    line = line.strip("\r\n\x00 ")
                    if not line:
                        continue
                    if "GalOS Benchmark Suite" in line:
                        in_bench = True
                    if in_bench:
                        results.append(line)
                    if in_bench and "Done" in line and "=====" in line:
                        in_bench = False
                        sent_exit = True

            elapsed = (datetime.datetime.now() - start).total_seconds()
            if elapsed > timeout:
                raise RuntimeError(f"Timeout after {timeout}s")

            if sent_exit:
                break

        s.close()
        print_summary(results)

    except Exception as e:
        print(f"\n[!] Error: {e}")
        raise
    finally:
        try:
            qemu_proc.terminate()
            qemu_proc.wait(timeout=3)
        except Exception:
            qemu_proc.kill()
            qemu_proc.wait()


def print_summary(results):
    latency_re = re.compile(
        r'\[(\w+)\s*\]\s+(\S+)\s*:\s+(\d+)\s+iters,\s+'
        r'min=\s*([\d.]+)(\w+),\s+avg=\s*([\d.]+)(\w+),\s+max=\s*([\d.]+)(\w+)'
    )
    throughput_re = re.compile(
        r'\[(\w+)\s*\]\s+(\S+)\s*:.*=\s+([\d.]+)\s+MB/s'
    )

    parsed = []
    for line in results:
        m = latency_re.search(line)
        if m:
            parsed.append((m.group(1), m.group(2),
                          f"{m.group(6)}{m.group(7)}",
                          f"{m.group(4)}{m.group(5)}",
                          f"{m.group(8)}{m.group(9)}"))
            continue
        m = throughput_re.search(line)
        if m:
            parsed.append((m.group(1), m.group(2), f"{m.group(3)} MB/s", "", ""))

    if parsed:
        print("\n" + "=" * 65)
        print("  SUMMARY")
        print("=" * 65)
        print(f"{'Category':<10} {'Test':<22} {'Avg':>12} {'Min':>10} {'Max':>10}")
        print("-" * 65)
        for cat, name, avg, mn, mx in parsed:
            print(f"{cat:<10} {name:<22} {avg:>12} {mn:>10} {mx:>10}")
        print("=" * 65)


def main():
    parser = argparse.ArgumentParser(description="GalOS Benchmark Runner")
    parser.add_argument("--arch", default="riscv64",
                        choices=["riscv64", "aarch64", "loongarch64", "x86_64"])
    parser.add_argument("--timeout", type=int, default=300,
                        help="Max seconds to wait (default: 300)")
    parser.add_argument("--skip-build", action="store_true")
    parser.add_argument("--skip-inject", action="store_true")
    parser.add_argument("--skip-kernel", action="store_true")
    parser.add_argument("benchmarks", nargs="*",
                        help="Specific benchmarks to run (default: all)")
    args = parser.parse_args()

    if not args.skip_build:
        build_bench(args.arch)
    if not args.skip_inject:
        inject_into_rootfs(args.arch)
    if not args.skip_kernel:
        build_kernel(args.arch)
    run_qemu(args.arch, args.timeout, args.benchmarks)


if __name__ == "__main__":
    main()
