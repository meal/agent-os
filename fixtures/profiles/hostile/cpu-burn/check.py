"""Hostile check, threat row "CPU": eight busy-looping processes for 3 s.

The guest sees only its vCPUs (`nproc`); the host bounds Firecracker's CPU time by the vCPU
count and the jail's cpu.max. Prints one JSON line; exits 0 (the bound is measured on the host).
"""
import json
import os
import sys
import time

nproc = os.cpu_count()
end = time.monotonic() + 3
children = []
for _ in range(8):
    pid = os.fork()
    if pid == 0:
        while time.monotonic() < end:
            pass
        os._exit(0)
    children.append(pid)
for pid in children:
    os.waitpid(pid, 0)
print(json.dumps({"nproc": nproc, "affinity": len(os.sched_getaffinity(0)), "burners": len(children)}, sort_keys=True))
sys.exit(0)
