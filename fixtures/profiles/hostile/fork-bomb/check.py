"""Hostile check, threat row "processes": `while True: os.fork()` for 5 s.

Every process keeps forking (failed forks are retried) until 5 s after the start, then exits
1. RLIMIT_NPROC (256 for `check`) bounds it in the guest; the host sees only Firecracker's
own threads in the jail's cgroup (pids.max 64). The outcome is the kill or the exit 1.
"""
import os
import time

end = time.monotonic() + 5
print("fork-bomb: forking for 5 s", flush=True)
while time.monotonic() < end:
    try:
        os.fork()
    except OSError:
        pass
os._exit(1)
