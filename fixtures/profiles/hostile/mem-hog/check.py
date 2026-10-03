"""Hostile check, threat row "memory": the check tries to make itself unkillable, then
allocates twice the guest's memory and touches every page.

First it writes -1000 and 0 to its own /proc/self/oom_score_adj: the trampoline set 1000
while privileged, which is also the floor, so both writes must fail (EACCES) and the value
stay 1000. Those findings are printed (flushed) before the allocation. The outcome is the
kill: the guest kernel's OOM killer ends the check (exit_code null), the agent survives. If
the allocation ever succeeds the check prints that and exits 1.
"""
import errno
import json
import sys

findings = {}
for value in (-1000, 0):
    try:
        with open("/proc/self/oom_score_adj", "w") as f:
            f.write(str(value))
        findings[f"write_{value}"] = "written"
    except OSError as e:
        findings[f"write_{value}"] = errno.errorcode.get(e.errno, repr(e))
with open("/proc/self/oom_score_adj") as f:
    findings["oom_score_adj"] = int(f.read())
with open("/proc/meminfo") as f:
    mem_total_mib = next(int(line.split()[1]) for line in f if line.startswith("MemTotal:")) // 1024
# MemTotal is the guest's memory (worker_memory_mib) less what the kernel keeps.
findings["target_mib"] = target = 2 * mem_total_mib
print(json.dumps(findings, sort_keys=True), flush=True)

chunk = 16 * 1024 * 1024
held = []
for _ in range(target * 1024 * 1024 // chunk):
    # Filled with ones, so every page is really touched.
    held.append(bytearray(b"\x01") * chunk)
print(json.dumps({"allocated_mib": len(held) * 16}), flush=True)
sys.exit(1)
