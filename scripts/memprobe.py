#!/usr/bin/env python3
"""Where a running Kourier's memory goes: sums `/proc/<pid>/smaps` by kind and prints one table.

Read-only. It reads only the memory map, the status and a few named environment variables of the
user's own Kourier process, plus the sizes of the files in the plaintext media cache
(`$XDG_RUNTIME_DIR/kourier/media`, tmpfs: RAM that no process's Rss shows). It prints no path, no
file name and nothing a message holds; the groups are fixed names.

    python3 -I scripts/memprobe.py                  # one snapshot of the one running kourier
    python3 -I scripts/memprobe.py --pid 1234       # when several run
    python3 -I scripts/memprobe.py --every 10 --hours 8 --csv mem.csv   # a line every 10 min

Units: MiB. Rss counts shared library pages in full, Pss splits them among the processes that map
them, Pss_Anon (the total) and Anonymous (per group) are the process's own heap-like memory (what an
allocator change moves), Private is what only this process holds, Swap what the kernel moved out.
"""

import argparse
import csv
import dataclasses
import os
import re
import sys
import time

GROUPS = [
    "heap ([heap])",
    "thread arenas",
    "anon <1 MiB",
    "anon 1-8 MiB",
    "anon 8-64 MiB",
    "anon >=64 MiB",
    "stacks",
    "kourier binary",
    "Qt libraries",
    "Qt plugins + QML",
    "KDE libraries",
    "GPU driver",
    "GPU/dma buffers",
    "shared memory",
    "other libraries",
    "fonts",
    "icons + caches",
    "locale",
    "database files",
    "other files",
    "kernel",
]
FIELDS = ["Size", "Rss", "Pss", "Anonymous", "Private", "Swap"]
GPU = re.compile(
    r"(nvidia|libGLX|libGLdispatch|libEGL|libGL\.|libGLESv|libgbm|libdrm|libgallium|mesa|radeon|amdgpu"
    r"|iris|i915|crocus|nouveau|libvulkan|vulkan|libLLVM|libdricore|_dri\.so|/dri/)",
    re.I,
)
HEADER = re.compile(r"^([0-9a-f]+)-([0-9a-f]+) (\S{4}) \S+ \S+ \d+\s*(.*)$")
ARENA = 64 << 20


def find_pid():
    uid = os.getuid()
    found = []
    for name in os.listdir("/proc"):
        if not name.isdigit():
            continue
        try:
            with open(f"/proc/{name}/comm") as f:
                comm = f.read().strip()
            if comm == "kourier" and os.stat(f"/proc/{name}").st_uid == uid:
                found.append(int(name))
        except OSError:
            continue
    if len(found) == 1:
        return found[0]
    if not found:
        sys.exit("memprobe: no kourier process of this user runs (start Kourier, or pass --pid)")
    sys.exit("memprobe: several kourier processes run, pick one with --pid: " + " ".join(map(str, found)))


def kind(path, exe, lo, size, perms):
    base = path.rsplit("/", 1)[-1]
    if path == "[heap]":
        return "heap ([heap])"
    if path.startswith("[stack"):
        return "stacks"
    if path.startswith("["):
        return "kernel"
    if path.startswith("anon_inode:"):
        return "GPU/dma buffers" if re.search(r"(gem|dmabuf|drm|kgsl|sync_file)", path) else "kernel"
    if path.startswith("/dev/dri") or path.startswith("/dev/nvidia"):
        return "GPU/dma buffers"
    if path.startswith(("/memfd:", "memfd:", "/dev/shm", "/SYSV")):
        return "shared memory"
    if not path:
        # glibc puts a thread's arena in a 64 MiB aligned reservation (its rw part plus a ---p rest)
        if lo % ARENA == 0 and 132 << 10 <= size <= ARENA and perms.startswith("rw"):
            return "thread arenas"
        if size < 1 << 20:
            return "anon <1 MiB"
        if size < 8 << 20:
            return "anon 1-8 MiB"
        if size < 64 << 20:
            return "anon 8-64 MiB"
        return "anon >=64 MiB"
    if exe and path == exe:
        return "kourier binary"
    if "/qt6/plugins/" in path or "/qt6/qml/" in path or base.endswith(".qmlc"):
        return "Qt plugins + QML"
    if base.startswith("libQt6"):
        return "Qt libraries"
    if base.startswith(("libKF6", "libKirigami", "libkirigami")) or "/kf6/" in path:
        return "KDE libraries"
    if GPU.search(path):
        return "GPU driver"
    if ".so" in base:
        return "other libraries"
    if base.endswith((".ttf", ".otf", ".ttc", ".pcf", ".pcf.gz", ".pfb")) or "/fontconfig/" in path:
        return "fonts"
    if "/icons/" in path or base.endswith((".cache", ".kcache")) or "/.cache/" in path:
        return "icons + caches"
    if "/locale" in path or base.endswith(".mo"):
        return "locale"
    if base.endswith((".db", "-shm", "-wal", ".sqlite")):
        return "database files"
    return "other files"


def read_maps(pid):
    try:
        exe = os.readlink(f"/proc/{pid}/exe")
    except OSError:
        exe = None
    sums = {g: dict.fromkeys(FIELDS, 0) for g in GROUPS}
    arenas = 0
    cur = None
    with open(f"/proc/{pid}/smaps") as f:
        for line in f:
            m = HEADER.match(line)
            if m:
                lo, hi = int(m.group(1), 16), int(m.group(2), 16)
                g = kind(m.group(4).strip(), exe, lo, hi - lo, m.group(3))
                if g == "thread arenas":
                    arenas += 1
                cur = sums[g]
                continue
            key, _, rest = line.partition(":")
            if cur is None or not rest.strip().endswith("kB"):
                continue
            kb = int(rest.split()[0])
            if key in ("Size", "Rss", "Pss", "Anonymous", "Swap"):
                cur[key] += kb
            elif key in ("Private_Clean", "Private_Dirty"):
                cur["Private"] += kb
    return sums, arenas


def rollup(pid):
    out = {}
    with open(f"/proc/{pid}/smaps_rollup") as f:
        for line in f:
            key, _, rest = line.partition(":")
            if rest.strip().endswith("kB"):
                out[key] = int(rest.split()[0])
    out["Private"] = out.get("Private_Clean", 0) + out.get("Private_Dirty", 0)
    return out


def status(pid):
    out = {}
    with open(f"/proc/{pid}/status") as f:
        for line in f:
            key, _, rest = line.partition(":")
            if key in ("Threads", "VmHWM", "VmRSS"):
                out[key] = int(rest.split()[0])
    return out


def uptime_h(pid):
    with open(f"/proc/{pid}/stat") as f:
        start = int(f.read().rsplit(")", 1)[1].split()[19])
    with open("/proc/uptime") as f:
        up = float(f.read().split()[0])
    return max(0.0, (up - start / os.sysconf("SC_CLK_TCK")) / 3600)


def named_env(pid):
    wanted = ("MALLOC_ARENA_MAX", "QSG_RHI_BACKEND", "QT_QUICK_BACKEND", "QT_QPA_PLATFORM", "XDG_SESSION_TYPE")
    out = {}
    try:
        with open(f"/proc/{pid}/environ", "rb") as f:
            for item in f.read().split(b"\0"):
                key, _, value = item.partition(b"=")
                if key.decode(errors="replace") in wanted:
                    out[key.decode()] = value.decode(errors="replace")[:40]
    except OSError:
        pass
    return out


def db_fds(pid):
    n = 0
    try:
        for fd in os.listdir(f"/proc/{pid}/fd"):
            try:
                if os.readlink(f"/proc/{pid}/fd/{fd}").endswith(".db"):
                    n += 1
            except OSError:
                continue
    except OSError:
        pass
    return n


def media_cache():
    run = os.environ.get("XDG_RUNTIME_DIR") or f"/run/user/{os.getuid()}"
    root = os.path.join(run, "kourier", "media")
    files, kb = 0, 0
    for dirpath, _dirs, names in os.walk(root):
        for name in names:
            try:
                st = os.lstat(os.path.join(dirpath, name))
            except OSError:
                continue
            files += 1
            kb += st.st_blocks // 2
    return files, kb


def mib(kb):
    return f"{kb / 1024:8.1f}"


@dataclasses.dataclass
class Snapshot:
    sums: dict
    arenas: int
    roll: dict
    st: dict
    up: float
    media: tuple


def snapshot(pid):
    sums, arenas = read_maps(pid)
    return Snapshot(sums, arenas, rollup(pid), status(pid), uptime_h(pid), media_cache())


def print_table(pid, s):
    try:
        with open(f"/proc/{pid}/comm") as f:
            comm = f.read().strip()
    except OSError:
        comm = "?"
    print(f"{comm} pid {pid}, running {s.up:.1f} h, {s.st.get('Threads', 0)} threads, "
          f"{s.arenas} thread arenas, {db_fds(pid)} open .db files, {os.cpu_count()} CPUs")
    env = named_env(pid)
    print("environment: " + (", ".join(f"{k}={v}" for k, v in sorted(env.items())) or "none of the watched variables set"))
    r = s.roll
    print(f"total MiB: Rss {mib(r.get('Rss', 0)).strip()}, Pss {mib(r.get('Pss', 0)).strip()}, "
          f"Pss_Anon {mib(r.get('Pss_Anon', 0)).strip()}, Pss_File {mib(r.get('Pss_File', 0)).strip()}, "
          f"Pss_Shmem {mib(r.get('Pss_Shmem', 0)).strip()}, Private {mib(r.get('Private', 0)).strip()}, "
          f"Swap {mib(r.get('Swap', 0)).strip()}, peak Rss {mib(s.st.get('VmHWM', 0)).strip()}")
    files, kb = s.media
    print(f"media cache (tmpfs, not in Rss): {files} files, {mib(kb).strip()} MiB")
    print()
    print(f"{'group':<18}" + "".join(f"{f:>10}" for f in FIELDS))
    for g in GROUPS:
        row = s.sums[g]
        if row["Size"] == 0:
            continue
        print(f"{g:<18}" + "".join(f"  {mib(row[f])}" for f in FIELDS))


def main():
    ap = argparse.ArgumentParser(description="Where a running Kourier's memory goes (read-only).")
    ap.add_argument("--pid", type=int)
    ap.add_argument("--every", type=float, metavar="MIN", help="sample every MIN minutes")
    ap.add_argument("--hours", type=float, default=8.0, help="with --every: stop after this many hours")
    ap.add_argument("--csv", default="kourier-mem.csv", help="with --every: the file the lines go to")
    a = ap.parse_args()
    pid = a.pid or find_pid()
    try:
        s = snapshot(pid)
    except PermissionError:
        sys.exit("memprobe: cannot read that process's memory map (another user's process?)")
    print_table(pid, s)
    if not a.every:
        return
    cols = ["time", "uptime_h", "threads", "rss", "pss", "pss_anon", "private", "swap", "media_mib"] + [
        g for g in GROUPS
    ]
    end = time.time() + a.hours * 3600
    with open(a.csv, "a", newline="") as f:
        w = csv.writer(f)
        if f.tell() == 0:
            w.writerow(cols)
        while True:
            r = s.roll
            line = [time.strftime("%Y-%m-%d %H:%M"), f"{s.up:.2f}", s.st.get("Threads", 0)]
            line += [f"{r.get(k, 0) / 1024:.1f}" for k in ("Rss", "Pss", "Pss_Anon", "Private", "Swap")]
            line += [f"{s.media[1] / 1024:.1f}"] + [f"{s.sums[g]['Pss'] / 1024:.1f}" for g in GROUPS]
            w.writerow(line)
            f.flush()
            print(f"{line[0]}  Pss {line[4]}  Pss_Anon {line[5]}  Rss {line[3]}  -> {a.csv}", flush=True)
            if time.time() + a.every * 60 > end:
                break
            time.sleep(a.every * 60)
            try:
                s = snapshot(pid)
            except (FileNotFoundError, ProcessLookupError):
                print("memprobe: Kourier ended")
                break


if __name__ == "__main__":
    main()
