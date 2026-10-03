#!/bin/sh
# mmdebstrap customize hook for the python-stdlib-v1 guest image.
# Called by scripts/build-guest-image.sh as: sh hooks/customize.sh ROOT AGENTOS_GUEST_BIN
#   ROOT               the chroot mmdebstrap built ("$1" of the hook)
#   AGENTOS_GUEST_BIN  the static musl agentos-guest to install as PID 1
# Runs as root in the build container with SOURCE_DATE_EPOCH exported.
set -eu
ROOT=${1:?usage: customize.sh ROOT AGENTOS_GUEST_BIN}
GUEST_BIN=${2:?usage: customize.sh ROOT AGENTOS_GUEST_BIN}

# The agent is the VM's PID 1 (kernel argument init=/sbin/agentos-guest); /sbin/init points at
# it too. /sbin is the merged-/usr symlink to usr/sbin.
install -m 0755 -o root -g root "$GUEST_BIN" "$ROOT/sbin/agentos-guest"
ln -s agentos-guest "$ROOT/sbin/init"

# builder (1000) owns /workspace and runs git; check (1001) runs the verification. Neither can
# log in. useradd comes from `passwd`, which --variant=apt does not install: the image's own
# useradd is used when it is there, the build container's `useradd --root` otherwise (both
# write the same /etc/passwd, /etc/group and /etc/shadow lines).
add_user() {
  if [ -x "$ROOT/usr/sbin/useradd" ]; then
    chroot "$ROOT" useradd -u "$1" -U -M -s /usr/sbin/nologin "$2"
  else
    useradd --root "$ROOT" -u "$1" -U -M -s /usr/sbin/nologin "$2"
  fi
}
add_user 1000 builder
add_user 1001 check

# Mount points (the drives and the runtime tmpfs). /run and /tmp exist in a Debian base.
mkdir -p "$ROOT/workspace" "$ROOT/scratch" "$ROOT/run" "$ROOT/tmp"
chmod 0755 "$ROOT/workspace" "$ROOT/scratch" "$ROOT/run"
chmod 1777 "$ROOT/tmp"

# No set-uid/set-gid program in the image: the check runs with no_new_privs already, and the
# image carries no privilege a process could gain by exec'ing a file.
find "$ROOT" -xdev -type f \( -perm -4000 -o -perm -2000 \) -exec chmod ug-s {} +

# Nothing that differs between builds or that the VM does not need. useradd's lastlog and
# faillog live in /var/log, so this comes after the users.
rm -rf "$ROOT"/var/cache/apt/* "$ROOT"/var/lib/apt/lists/* "$ROOT"/var/log/* \
  "$ROOT/etc/machine-id" "$ROOT/usr/share/doc" "$ROOT/usr/share/man" "$ROOT/usr/share/locale" \
  "$ROOT/etc/resolv.conf"
# The *-old backups dpkg and debconf leave behind and ldconfig's cache of file times.
rm -f "$ROOT"/var/lib/dpkg/*-old "$ROOT"/var/cache/debconf/*-old "$ROOT/var/cache/ldconfig/aux-cache" \
  "$ROOT/etc/passwd-" "$ROOT/etc/group-" "$ROOT/etc/shadow-" "$ROOT/etc/gshadow-"

echo agentos-guest > "$ROOT/etc/hostname"
# No network configuration: no resolv.conf (removed above), no interfaces file, no hosts entry
# beyond what base-files ships. The VM has no NIC (network-interfaces: []).
rm -f "$ROOT/etc/network/interfaces"
rm -rf "$ROOT/etc/network/interfaces.d"
