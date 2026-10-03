"""Hostile check, threat row "disk": the check fills every place it can write and tries to
write where it must not.

1 MiB blocks into /tmp/fill (tmpfs, 64 MiB) and /scratch/check/fill (the 512 MiB scratch
drive) until ENOSPC; a new file in the workspace (expects EACCES: /workspace belongs to
`builder`); a new file in / and the root's flags (read-only squashfs); `mount -o
remount,rw /` through the binary and through the syscall (both must fail). Prints one JSON
line of byte counts and results; exits 0 when every bound held, 1 otherwise.
"""
import ctypes
import errno
import json
import os
import subprocess
import sys

MIB = 1 << 20
workspace = sys.argv[1] if len(sys.argv) > 1 else "/workspace"


def name(e):
    return errno.errorcode.get(e.errno, repr(e)) if getattr(e, "errno", None) else repr(e)


def fill(path):
    block = b"\xa5" * MIB
    written = 0
    try:
        with open(path, "wb", buffering=0) as f:
            while True:
                written += f.write(block)
    except OSError as e:
        result = name(e)
    try:
        os.remove(path)
    except OSError:
        pass
    return written, result


findings = {}
findings["tmp_bytes"], findings["tmp"] = fill("/tmp/fill")
findings["scratch_bytes"], findings["scratch"] = fill("/scratch/check/fill")
for label, path in (("workspace", os.path.join(workspace, "x")), ("root", "/x")):
    try:
        open(path, "w").close()
        findings[label] = "created"
    except OSError as e:
        findings[label] = name(e)
findings["root_ro"] = bool(os.statvfs("/").f_flag & os.ST_RDONLY)
findings["workspace_owner"] = os.stat(workspace).st_uid
try:
    done = subprocess.run(["mount", "-o", "remount,rw", "/"], capture_output=True)
    findings["remount_rc"] = done.returncode
except OSError as e:
    findings["remount_rc"] = name(e)
libc = ctypes.CDLL(None, use_errno=True)
MS_REMOUNT = 32
rc = libc.mount(None, b"/", None, MS_REMOUNT, None)
findings["remount_syscall"] = "succeeded" if rc == 0 else errno.errorcode.get(ctypes.get_errno(), "?")
findings["root_ro_after"] = bool(os.statvfs("/").f_flag & os.ST_RDONLY)

held = (
    findings["tmp"] == "ENOSPC" and 0 < findings["tmp_bytes"] <= 64 * MIB
    and findings["scratch"] == "ENOSPC" and 0 < findings["scratch_bytes"] <= 512 * MIB
    and findings["workspace"] == "EACCES"
    and findings["root"] in ("EROFS", "EACCES")
    and findings["root_ro"] and findings["root_ro_after"]
    # The image may carry no `mount` binary (ENOENT); the syscall is tried either way.
    and findings["remount_rc"] != 0
    and findings["remount_syscall"] == "EPERM"
)
print(json.dumps(findings, sort_keys=True))
sys.exit(0 if held else 1)
