"""Hostile check, threat rows "secrets" and "handles": the check looks everywhere it can
for host secrets and for a way back to the host.

The test plants NAME=<random hex> in the host-side processes' environment and in
<home>/secret.txt, NAME being MARKER below. The check does not know the value: it searches for
the name (spelt in two pieces, so this file never matches itself). It reads
the kernel command line, every /proc/*/environ it can open, every regular file of every
mounted filesystem (bounded by time and size), the raw drives /dev/vd* (expects EACCES) and
/dev/vsock (expects EACCES); it tries a vsock connection to the host (CID 2); and it records
what it is: uid/gid, groups, no_new_privs, rlimits, oom_score_adj, open fds, setuid(0).
Prints one JSON line; exits 0 when nothing was found and every protection held, 1 otherwise.
"""
import errno
import json
import os
import resource
import signal
import socket
import stat
import sys
import time

MARKER = b"AGENTOS_TEST" + b"_SECRET"
FILE_LIMIT = 1 << 20
BUDGET_SECONDS = 25
started = time.monotonic()
hits = []
stats = {"files_read": 0, "bytes_read": 0, "environ_read": 0, "truncated": False}
findings = {}
# Before anything of ours is open: every descriptor the check was started with. The
# listing's own descriptor is closed by the time its entry is read back (readlink fails).
findings["fd_targets"] = {}
for fd in sorted(int(fd) for fd in os.listdir("/proc/self/fd")):
    try:
        findings["fd_targets"][fd] = os.readlink(f"/proc/self/fd/{fd}")
    except FileNotFoundError:
        pass
findings["fds"] = sorted(findings["fd_targets"])


def name(e):
    return errno.errorcode.get(e.errno, repr(e)) if getattr(e, "errno", None) else repr(e)


class Slow(Exception):
    pass


def on_alarm(_sig, _frame):
    raise Slow()


signal.signal(signal.SIGALRM, on_alarm)


def scan(label, data):
    i = data.find(MARKER)
    if i >= 0:
        hits.append(f"{label}: {data[max(0, i - 40):i + 80]!r}")


def read_file(path, limit=FILE_LIMIT):
    # A read that blocks (a pipe-like file in /proc or /sys) is abandoned after 1 s.
    signal.setitimer(signal.ITIMER_REAL, 1.0)
    try:
        with open(path, "rb", buffering=0) as f:
            return f.read(limit)
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)


scan("/proc/cmdline", read_file("/proc/cmdline"))
for pid in [p for p in os.listdir("/proc") if p.isdigit()]:
    try:
        data = read_file(f"/proc/{pid}/environ")
    except (OSError, Slow):
        continue
    stats["environ_read"] += 1
    scan(f"/proc/{pid}/environ", data)

with open("/proc/mounts") as f:
    mount_points = sorted({line.split()[1] for line in f})
seen = set()
for top in mount_points:
    try:
        top_dev = os.stat(top).st_dev
    except OSError:
        continue
    for dirpath, dirnames, filenames in os.walk(top, onerror=lambda e: None):
        # Each filesystem once (its own walk), never /proc/<pid> trees (every process's
        # view of everything again) nor the kernel's never-ending files.
        dirnames[:] = [
            d for d in dirnames
            if not (dirpath == "/proc" and d.isdigit())
            and not (os.path.join(dirpath, d) in mount_points and os.path.join(dirpath, d) != top)
        ]
        for fname in filenames:
            path = os.path.join(dirpath, fname)
            if time.monotonic() - started > BUDGET_SECONDS:
                stats["truncated"] = True
                break
            if path in ("/proc/kmsg", "/proc/kcore") or path.startswith("/sys/kernel/debug"):
                continue
            try:
                st = os.lstat(path)
                if not stat.S_ISREG(st.st_mode) or st.st_dev != top_dev or (st.st_dev, st.st_ino) in seen:
                    continue
                seen.add((st.st_dev, st.st_ino))
                data = read_file(path)
            except (OSError, Slow):
                continue
            stats["files_read"] += 1
            stats["bytes_read"] += len(data)
            scan(path, data)
        if stats["truncated"]:
            break

findings.update({"hits": hits, **stats})

drives = sorted(d for d in os.listdir("/dev") if d.startswith("vd"))
results = set()
for d in drives:
    try:
        read_file(f"/dev/{d}", 4096)
        results.add("read")
    except OSError as e:
        results.add(name(e))
findings["drives"] = drives
findings["blockdev"] = "/".join(sorted(results)) or "none"
try:
    open("/dev/vsock", "rb").close()
    findings["vsock"] = "opened"
except OSError as e:
    findings["vsock"] = name(e)
try:
    with socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM) as s:
        s.settimeout(2)
        s.connect((2, 5200))
        findings["vsock_connect"] = "connected"
except OSError as e:
    findings["vsock_connect"] = name(e)
findings["uid"], findings["gid"] = os.getuid(), os.getgid()
findings["euid"], findings["egid"] = os.geteuid(), os.getegid()
findings["groups"] = os.getgroups()
with open("/proc/self/status") as f:
    status = dict(line.rstrip("\n").split(":\t", 1) for line in f if ":\t" in line)
findings["no_new_privs"] = int(status.get("NoNewPrivs", "-1"))
findings["cap_eff"] = status.get("CapEff")
findings["nproc"] = list(resource.getrlimit(resource.RLIMIT_NPROC))
findings["nofile"] = list(resource.getrlimit(resource.RLIMIT_NOFILE))
with open("/proc/self/oom_score_adj") as f:
    findings["oom_score_adj"] = int(f.read())
try:
    os.setuid(0)
    findings["setuid0"] = "succeeded"
except OSError as e:
    findings["setuid0"] = name(e)

held = (
    not hits
    and findings["blockdev"] == "EACCES"
    and findings["vsock"] == "EACCES"
    and findings["vsock_connect"] != "connected"
    and findings["fds"] == [0, 1, 2]
    and (findings["uid"], findings["euid"], findings["gid"], findings["egid"]) == (1001, 1001, 1001, 1001)
    and findings["groups"] == []
    and findings["no_new_privs"] == 1
    and findings["cap_eff"] == "0000000000000000"
    and findings["nproc"] == [256, 256]
    and findings["nofile"] == [1024, 1024]
    and findings["oom_score_adj"] == 1000
    and findings["setuid0"] == "EPERM"
    and not findings["truncated"]
)
print(json.dumps(findings, sort_keys=True))
sys.exit(0 if held else 1)
