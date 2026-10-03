"""Hostile check, threat row "network": the check tries to reach the outside.

Isolation held: a TCP connection to 10.0.0.1:80 fails with ENETUNREACH (no route: the VM
has no NIC) and /sys/class/net lists exactly `lo`. Prints one JSON line of findings; exits 0
when the isolation held, 1 otherwise.
"""
import errno
import json
import os
import socket
import sys


def attempt(family, address):
    try:
        s = socket.socket(family, socket.SOCK_STREAM)
        s.settimeout(1)
        s.connect(address)
        s.close()
        return "connected"
    except OSError as e:
        if e.errno == errno.ENETUNREACH:
            return "unreachable"
        return errno.errorcode.get(e.errno, repr(e)) if e.errno else repr(e)


findings = {}
try:
    socket.create_connection(("10.0.0.1", 80), timeout=1).close()
    findings["net"] = "connected"
except OSError as e:
    findings["net"] = "unreachable" if e.errno == errno.ENETUNREACH else (errno.errorcode.get(e.errno) or repr(e))
# The same, by IPv6 and towards a public address: no route either way.
findings["net6"] = attempt(socket.AF_INET6, ("2001:db8::1", 80))
findings["public"] = attempt(socket.AF_INET, ("1.1.1.1", 443))
findings["ifaces"] = sorted(os.listdir("/sys/class/net"))
held = (
    findings["net"] == "unreachable"
    and findings["public"] == "unreachable"
    and findings["net6"] in ("unreachable", "EADDRNOTAVAIL", "EAFNOSUPPORT")
    and findings["ifaces"] == ["lo"]
)
print(json.dumps(findings, sort_keys=True))
sys.exit(0 if held else 1)
