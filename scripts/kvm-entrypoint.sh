#!/bin/sh
# Entrypoint of the compose `test-kvm` service: gives the jailer a cgroup v2 tree it can use,
# then runs the command. ("Jailer experiments" E2, E4, E5 in
# docs/superpowers/specs/2026-10-02-phase-3b-firecracker-worker-design.md.)
#
# cgroup v2's "no internal process" rule: a cgroup that has processes of its own cannot
# enable controllers for its children (writing cgroup.subtree_control fails with EBUSY).
# The container's processes all start in the root of its cgroup namespace, so they move to a
# leaf `init/` first; then the root can delegate cpu, memory and pids, and the jailer can
# create `agentos/<id>` with cpu.max, memory.max, memory.swap.max and pids.max.
set -eu

# Docker mounts the container's cgroup tree read-only (E2: the jailer's mkdir fails with
# EROFS); CAP_SYS_ADMIN (cap_add) allows the remount.
mount -o remount,rw /sys/fs/cgroup

# The leaf every process of the container lives in from now on (children inherit it).
mkdir -p /sys/fs/cgroup/init

# Move every process out of the root (docker-init and this shell, as `init: true` makes the
# entrypoint a child of docker-init). The list is read once; a pid in it may already be gone
# (the `cat` that printed it, for one): only that case is tolerated, any other failure stops
# here and the subtree_control write below would fail anyway.
for pid in $(cat /sys/fs/cgroup/cgroup.procs); do
  echo "$pid" > /sys/fs/cgroup/init/cgroup.procs 2>/dev/null || [ ! -d "/proc/$pid" ]
done

# Delegate the three controllers the jailer writes (E5: without this even a privileged
# container fails with "Not supported").
echo "+cpu +memory +pids" > /sys/fs/cgroup/cgroup.subtree_control

exec "$@"
