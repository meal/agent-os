# Phase 3b: Firecracker worker and guest workspace — design

Date: 2026-10-02
Status: **accepted by the owner on 2026-10-02** (the eight decisions and the approvals are recorded in "Owner decisions" at the end; the jailer moved from 3b-2 into 3b-1 by that decision). Environment facts and the jailer experiments below were measured on this host on 2026-10-02.
Parent spec: `Agent_OS_v1_Build_Plan.md` (Phase 3, "Authority and isolation"; rows "guest attempts network access or host secret access" and "repository code stays within configured CPU and memory").
Builds on: `docs/superpowers/specs/2026-10-02-phase-3a-supervisor-broker-design.md` and `docs/superpowers/plans/2026-10-02-phase-3a-supervisor-broker.md` (branch `phase-3a`, finished).

## Purpose and scope

3a made every effect a supervised job with a lease, a deadline, cancellation, revocation and a broker. Its workers are still host processes: repository code and the verification check run as the controller's own UID, with the host's network and filesystem. 3b puts a Firecracker microVM behind the existing `Worker` trait so that:

- repository code (the patch application and the verification check) runs inside a guest that has **no network device and nothing of the host but its own drives**;
- the guest's CPU and memory come from the contract (`worker_vcpus`, `worker_memory_mib`) and are enforced by the VM;
- the task workspace lives on a per-task block image that only the guest ever mounts, so `reconcile` and `current_workspace` are answered **by booting the guest in inspection mode**, not by the controller reading a host directory;
- the guest image is built reproducibly under `docker compose`, registered content-addressed and pinned like a verification profile;
- every 3a guarantee (lease kill, deadline, cancel marker, fencing, no orphan after a supervisor SIGKILL, kill-receipt rule, byte-compatible manifests) still holds, and the crash matrix and the CLI crash demo run unchanged against the new worker.

The host-process worker stays, is the default, and is what `docker compose run --rm test cargo test --workspace` exercises. The Firecracker worker is selected per task.

### Split: 3b-1 and 3b-2

3b is too large for one plan. This document designs both parts; **the first plan covers 3b-1 only**, and 3b-2 is designed here so 3b-1 does not paint it into a corner.

| Part | Contents |
| --- | --- |
| **3b-1** (first plan) | `agentos-guest` crate and guest image build; image registry and pinning; vsock control protocol with a fake guest for host-only tests; `FirecrackerWorker`; per-task workspace image; inspection boots for `reconcile`/`current_workspace`; vCPU and memory from the contract; no NIC; **the official `jailer` (chroot, uid/gid drop, cgroup v2 `cpu.max`/`memory.max`/`memory.swap.max`/`pids.max`, `fsize` limit, Firecracker's default seccomp) for every job VM and every inspector VM, with an explicit `--allow-unjailed` fallback recorded in `Submitted`**; worker selection and recording; KVM-gated test tier (`test-kvm` compose service with the settings that make the jailer work in a container); README. |
| **3b-2** (second plan) | block-device rate limiting; contract-driven disk sizes (`worker_disk_mib`); garbage collection of workspace and scratch images with the job directories; building the guest kernel from source (the build plan's Phase 6 "reproducible guest image build" finishes there); jailer extras that need a different supervision model or more privilege: `--new-pid-ns` (the jailer parent exits at once, see "The jail"), `--netns`, a per-job jail uid, the `userfaultfd` device. |

## Environment facts (measured 2026-10-02)

Host (`stark`, Arch Linux, kernel `7.2.7-arch1-1`, x86_64, 32 CPUs, 64 GiB):

- `/dev/kvm` exists: `crw-rw-rw- 1 root kvm 10, 232`; `open(O_RDWR)` succeeds for the dev user (uid 1000, not in `kvm`; the mode is world-rw). `/proc/cpuinfo` has `vmx`; `kvm_intel` and `kvm` are loaded; `systemd-detect-virt` says `none` (bare metal, **no nested virtualization needed**). `/dev/vhost-vsock` exists but is not needed (Firecracker implements vsock in user space).
- `firecracker` and `jailer` are not installed on the host; nothing was installed system-wide for this spec.
- Docker 29.8.2, Compose 5.5.1.

Compose `test` container (`rust:1.98.1-bookworm`, runs as root, `init: true`):

- **By default `/dev/kvm` is not visible** (`ls: cannot access '/dev/kvm'`).
- With an override file adding `devices: ["/dev/kvm:/dev/kvm"]` to the `test` service, `/dev/kvm` is visible and `open(O_RDWR)` succeeds. The container's cgroup v2 tree is mounted **read-only** (`/sys/fs/cgroup ... ro`, private cgroup namespace), the effective capability set is Docker's default (no `CAP_SYS_ADMIN`) and Docker's default seccomp profile applies; the jailer needs all three changed, and the `test-kvm` service settings that do so were found by experiment ("Jailer experiments" below). The `test` service itself does not change: `cargo test --workspace` keeps running without KVM, root privileges beyond the container's own, or a writable cgroup tree.
- Host: Docker uses the `systemd` cgroup driver on cgroup v2 (`cpuset cpu io memory hugetlb pids rdma misc dmem` delegated at the root); AppArmor is not enabled on this host; the host has a 64 GiB swap partition (relevant: a cgroup `memory.max` alone lets a VM spill into swap instead of being killed; measured below). `/home` and `/` are xfs without `nodev`/`noexec`; `/tmp` is tmpfs with `nodev` (a jail cannot live there: measured below).
- The container has `mkfs.ext4` (e2fsprogs), `git`, `python3`, `curl`; it lacks `mksquashfs`, `mmdebstrap`, `debootstrap` and the `x86_64-unknown-linux-musl` Rust target (only `x86_64-unknown-linux-gnu` is installed). The `Dockerfile` must add them (below).

Firecracker (GitHub releases API, 2026-10-02):

- Latest release **v1.17.0**, published 2026-09-10 (same day as v1.16.2; earlier: v1.16.1 2026-07-02, v1.16.0 2026-06-04, v1.15.1 2026-04-07). The 3a plan's "v1.17.0 as of 2026-10-02" is confirmed.
- `firecracker-v1.17.0-x86_64.tgz` sha256 `06094a1108ae9e82aa4c23a775aa92758f53f1175d422270d9d6162cb9ade558`; it contains the static musl `firecracker` (sha256 `99ad0f5cd0514a88aad0e9ae8cfdb3cc3b4ab9d190e1194602406c786b5de7a5`), `jailer` (`65ef226e96f0ceda55ba643f445801ef2cc0ea667ef67cad8ac4f406c9c8434f`), `seccompiler-bin`, `cpu-template-helper`, `snapshot-editor`, `rebase-snap`, the default seccomp filter JSON and CPU templates.
- Kernel support policy: validated host kernels 5.10, 6.1, 6.18; validated guest kernels 5.10, 6.1, 6.18 (6.18 supported until at least 2028-06-01). **Our host kernel 7.2 is newer than any validated host kernel** (risk, below); it worked in the smoke test.
- Recommended guest artifacts: the Firecracker CI bucket `https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/<date>-<sha>/x86_64/`; the newest prefix on 2026-10-02 is `firecracker-ci/20260930-a738f18a8db0-0/`, offering `vmlinux-5.10.268`, `vmlinux-6.1.186`, `vmlinux-6.18.51` (each with its `.config`) and `ubuntu-24.04.squashfs`. `vmlinux-6.18.51` sha256 `0545ba1781fc06cfa1d7699069057f4538103fd1644100cf0da434899a1ed447`; its config has `EXT4_FS=y`, `SQUASHFS=y` (zstd, xz), `OVERLAY_FS=y`, `TMPFS=y`, `DEVTMPFS=y`, `VIRTIO_BLK=y`, `VIRTIO_VSOCKETS=y`, `VIRTIO_MMIO=y`, `CGROUPS=y`, `MEMCG=y`, `PID_NS=y`, `USER_NS=y`, `SECCOMP=y`, `SERIAL_8250_CONSOLE=y`. Firecracker's docs recommend an uncompressed `vmlinux` on x86_64, boot args `console=ttyS0 reboot=k panic=1`, and note that a guest `reboot` makes Firecracker exit (no guest power management).
- `firecracker --no-api --config-file FILE` boots from one JSON (fields named as in the API: `boot-source`, `drives`, `machine-config`, `vsock`, `network-interfaces`, `logger`, …) and the process **exits when the guest shuts down** (the no-API loop breaks on `FcExitCode::Ok`). Exit codes: 0 ok, 1 generic error, 2 unexpected error, 148 bad syscall (seccomp), 149 SIGBUS, 150 SIGSEGV, 151 SIGXFSZ, 152 bad configuration, 153 argument parsing, 154 SIGXCPU, 155 SIGPIPE, 156 SIGHUP, 157 SIGILL. The default seccomp filter is applied with or without the jailer; `--no-seccomp`/`--seccomp-filter` are not recommended for production and are not used.
- API limits (swagger): `vcpu_count` 1..=32, `mem_size_mib` integer, `smt` default false, `huge_pages` default None; `Drive{drive_id, is_root_device, is_read_only, path_on_host, cache_type: Unsafe|Writeback (default Unsafe), io_engine: Sync|Async, rate_limiter}`; `Vsock{guest_cid >= 3, uds_path}`; `Balloon{amount_mib, deflate_on_oom}` (not used).
- vsock: the guest's `AF_VSOCK` ports map 1:1 to host `AF_UNIX` sockets. Host-initiated: connect to `uds_path`, send `CONNECT <port>\n`, read `OK <hostport>\n`; if nobody listens in the guest Firecracker closes the connection. Guest-initiated connections go to `uds_path_<port>` on the host (not used: the guest never initiates).
- Jailer (read from the v1.17.0 sources `src/jailer/src/{main,env,chroot,cgroup,resource_limits}.rs` and `docs/jailer.md` on 2026-10-02): must run as root. `--id` is 1–64 characters of `[A-Za-z0-9-]` (our attempt ids are UUID v4, `inspect-<uuid>` is 44 characters). It closes every fd above 2, clears the environment, creates `<chroot_base>/<exec_file_name>/<id>/root/` (**nothing is done if the path already exists**, so the caller may stage files there first), copies `--exec-file` into it as `<exec_file_name>` (refusing a destination that is a hard link), installs `RLIMIT_NOFILE` (default 2048) and, only if given, `RLIMIT_FSIZE`; **before** chrooting it creates the cgroup `<cgroup2 mount>/<parent_cgroup>/<id>` (`--cgroup-version 2`, `--parent-cgroup`, default parent = `<exec_file_name>`) and, for every `--cgroup file=value`, writes `+<controller>` into `cgroup.subtree_control` of **every ancestor up to the mount point** (`write_all_subtree_control` recurses until a directory without `cgroup.subtree_control`), writes the value, then adds its own pid to `cgroup.procs`; then `unshare(CLONE_NEWNS)`, `mount(/, MS_SLAVE|MS_REC)`, bind-mounts the chroot dir onto itself, `mkdir old_root` (0700), `pivot_root(., old_root)`, `umount2(old_root, MNT_DETACH)`, `rmdir old_root`; creates and chowns `/dev`, `/dev/net`, `/run`; `mknod`s `/dev/net/tun` (10,200), `/dev/kvm` (10,232), `/dev/urandom` (warning only on failure) and `/dev/userfaultfd` if the host has it; chowns the chroot root (0700) and the devices to `--uid/--gid`; writes `firecracker.pid` in the chroot **in both modes**; then `setuid/setgid` and `exec`s `/<exec_file_name> --id <id> --start-time-us … --start-time-cpu-us … --parent-cpu-time-us … <arguments after -->` with stdio **inherited** (so a caller's redirections and process group survive; no `setsid` unless `--daemonize` or `--new-pid-ns`). **With `--new-pid-ns` the jailer `clone(CLONE_NEWPID)`s, writes the child pid and the parent `exit(0)`s immediately**: the caller's child is gone and Firecracker is reparented, so its exit status cannot be waited for; with `--daemonize` it double-forks and redirects stdio to `/dev/null`. Neither is usable under the 3a supervisor (below).

**Smoke test (real, in the compose container with `/dev/kvm` passed through):** `firecracker --no-api --config-file vm.json` with v1.17.0, `vmlinux-6.18.51`, 1 vCPU, 128 MiB, a read-only 8 MiB ext4 root drive, a vsock device and no network interface: the kernel booted (`Linux version 6.18.51+`, `virtio_blk virtio0: [vda]`, `NET: Registered PF_VSOCK`), mounted the root read-only, ran `/sbin/init`, panicked "No working init found", rebooted after 1 s and **Firecracker exited 0 after 1649 ms** ("Firecracker exiting successfully. exit_code=0"); the host socket `v.sock` had been created in the config directory. Firecracker appended `root=/dev/vda ro` and the `virtio_mmio.device=` entries to the command line itself. So Firecracker can run in the dev environment **when** the compose service gets `/dev/kvm`; nothing else (no nested virt, no root on the host, no jailer) is required.

Crate versions (`cargo search`, 2026-10-02; `cargo search` sends no identity): `vsock` 0.5.4 (guest side, `AF_VSOCK` sockets), `rustix` 1.1.5 (already used; `mount`, `process`, `fs` features for the guest init), `base64` 0.23.1 (the `*_b64` protocol fields; owner-approved new dependency), `tokio-vsock` 0.7.2 (not used: the guest agent is synchronous), `firec` 0.2.0 (not used: with `--no-api` no API client is needed), `hyper` 1.11.1 / `hyperlocal` 0.9.1 (not used, same reason). Re-check before pinning.

### Jailer experiments (measured 2026-10-02)

Setup: the compose image (`rust:1.98.1-bookworm`, root, `init: true`), the v1.17.0 `firecracker` and `jailer` from the sha256-verified release tarball, `vmlinux-6.18.51`, an 8 MiB ext4 root whose `/sbin/init` is a static C program (writes a line to the console, writes a marker into `/dev/vdb`, `sync`, `reboot`), a sparse 64 MiB `ws.img`, 1 vCPU, 256 MiB. The jail "home" was on the container's overlay root (one filesystem, no `nodev`); every file the jail needs was **hard-linked** into `<job>/jail/firecracker/<id>/root/` and `ws.img` was `chown`ed to uid 61000 first. The jailer argv was the one in "The jail" below. Nothing was installed in the repository or system-wide; every artifact lived in the session scratch directory.

| # | Container settings | Jailer arguments | Outcome |
| --- | --- | --- | --- |
| E1 | `test` + `devices: [/dev/kvm]` | none (plain `firecracker --no-api`) | boots, guest line printed, marker visible in `ws.img`, exit 0 after **600 ms** |
| E2 | same | `--cgroup cpu.max/memory.max/pids.max` | exit 1: `Failed to create directory /sys/fs/cgroup/agentos/<id>: Read-only file system (os error 30)`; the half-built chroot (binary copy, 3.7 MB) is left behind |
| E2b | same | no `--cgroup` | exit 1: `Failed to unshare into new mount namespace: Operation not permitted` (no `CAP_SYS_ADMIN`) |
| E3 | + `cap_add: [SYS_ADMIN]` | with / without `--cgroup` | with: still `Read-only file system`; without: `Failed to pivot root: Operation not permitted` — Docker's default seccomp profile is deny-by-default and **does not list `pivot_root`** at all (`mount`, `unshare`, `umount2`, `setns` are allowed with `CAP_SYS_ADMIN`; verified in `moby/profiles` `seccomp/default.json`) |
| E4 | + entrypoint (`mount -o remount,rw /sys/fs/cgroup`; move every pid to `init/`; `+cpu +memory +pids` into the root's `cgroup.subtree_control`) | `--cgroup …` | the cgroup `agentos/<id>` is created with `cpu.max`, `memory.max`, `pids.max` set; then `pivot_root: EPERM` as in E3 |
| E5 | `privileged: true`, no entrypoint | `--cgroup …` | exit 1: `Failed to write to /sys/fs/cgroup/agentos/cgroup.subtree_control: Not supported (os error 95)` — privilege does not replace controller delegation |
| E10 | cap + entrypoint + `seccomp=unconfined` | full argv | **boots jailed**: exit 0 after 594 ms; process uid/gid 61000; cmdline `/firecracker --id <id> --start-time-us … --no-api --config-file /vm.json`; `/proc/<pid>/cgroup` = `0::/agentos/<id>` with the pid in `cgroup.procs`; **same pgid and sid as the spawning shell** (exec in place, pid unchanged); `firecracker.pid` and `v.sock` in the chroot; the guest's marker is in `<job>/ws.img` through the hard link; `memory.peak` 70 MB; after exit `cgroup.procs` is empty, `rmdir` of the cgroup and `rm -rf` of the chroot succeed |
| E11 | cap + entrypoint + `scripts/kvm-seccomp.json` (a 49-syscall deny list: host-dangerous calls, a subset of what Docker's default profile gates behind a capability, a kernel version or outright; `pivot_root` not among them) | full argv | identical to E10 (567 ms). **This is the chosen setting.** |
| E6 | E11 settings; guest touches 208 MiB of its 256 MiB | `memory.max=96 MiB` | **not killed**: `memory.events max 2392`, `memory.peak` = the limit, the excess went to the host's swap partition, the run slowed from 1.7 s to 2.2 s |
| E6c | same | `memory.max=96 MiB` **and `memory.swap.max=0`** | Firecracker **OOM-killed by the host** after the guest touched 32 MiB: `memory.events oom 1 oom_kill 1`, wait status 137 (SIGKILL); cgroup empty, `rmdir` ok |
| E6b/E6d | same guest | `memory.max=384 MiB` (± `memory.swap.max=0`) | survives; `memory.peak` 273 MB / 268 MB for a 256 MiB guest ⇒ Firecracker's own overhead ≈ 45–60 MB at full guest occupancy; `VmRSS` 59–72 MB early in the boot |
| E7 | guest busy-loops 3 s (guest clock) | `cpu.max=50000 100000` | guest wall 3.09 s, `cpu.stat usage_usec 1,357,986`, `nr_throttled 26`, `throttled_usec 1,269,459` — the cgroup halves the CPU the guest gets |
| E7b | same | `cpu.max=100000 100000` (the design value for 1 vCPU) | `usage_usec 2,561,214` over 3.05 s, `nr_throttled 2`, 1 ms throttled — the design quota does not slow a saturating guest |
| E8 | SIGKILL the jailed process 0.3 s after spawn | full argv | wait status 137; `cgroup.procs` empty at once; `rmdir` of the cgroup and removal of the chroot succeed — leftovers are inert and collectable |
| E9 | jail base on a `nodev` tmpfs | full argv | the jailer succeeds, Firecracker fails: `Kvm error: Error creating KVM object: Permission denied (os error 13)`, exit 1 — the jail must be on a filesystem mounted without `nodev` (and without `noexec`: the binary is exec'd from the chroot) |
| E12 | the exact `test-kvm` snippet below (`extends: test`, `cap_add`, `security_opt: [seccomp=./scripts/kvm-seccomp.json, apparmor=unconfined]`, `entrypoint`) | full argv | jailed boot as E11 (607 ms); `apparmor=unconfined` is accepted on a host without AppArmor; the `test` service in the same file is unchanged (cgroup `ro`, no `/dev/kvm`, default capabilities) |

Conclusions carried into the design: the jailer exec's in place (same pid, pgid, sid, stdio), so the 3a supervisor semantics hold without change; `--new-pid-ns` and `--daemonize` cannot be used; the container needs `CAP_SYS_ADMIN`, a seccomp profile that allows `pivot_root`, a writable cgroup root with delegated controllers and `/dev/kvm`, nothing more (no `privileged`, no host cgroup namespace); `memory.swap.max=0` is part of the memory bound; the jail directory must be on the same filesystem as `ws.img` and the image (hard links) and that filesystem must allow device nodes and execution; a job killed by SIGKILL leaves an empty cgroup and a chroot that are removable by root.

## Non-goals

Model broker (Phase 4); the Wasm component ABI (Phase 5); multi-host or remote workers; VM snapshots, restore and live migration; a long-lived VM per task or a VM pool; GPU; a guest network of any kind (not even a host-only tap); guest-initiated vsock connections; aarch64; a PID or network namespace for the Firecracker process (`--new-pid-ns`, `--netns`: 3b-2, see "The jail"); a per-job jail uid (3b-2; one `JAIL_UID` per home in 3b-1); block-device rate limiting (3b-2); image garbage collection (3b-2; `<home>/jobs` GC remains the 3a known limit, but the jail of a job — chroot and cgroup — **is** collected); `worker_disk_mib` in the contract (3b-2); building the guest kernel from source (3b-2); running the jailer anywhere but as root (the jailer itself requires it; the unjailed fallback exists for that case); any change to the journal schema, reducer, broker rules, lease arithmetic, or the job-directory liveness rule.

## Decisions at a glance

| Question | Decision | Rejected |
| --- | --- | --- |
| Where Firecracker runs | As the **worker process's child**, in the worker's own process group, under the unchanged 3a supervisor. | Supervisor spawning Firecracker directly (changes the supervisor and the kill paths for one worker); a VM daemon (3a: no daemon). |
| VM granularity | **One microVM per job** (effect attempt), booted fresh, destroyed when the job ends. | One VM per task (needs an owner outliving jobs and controllers; snapshot/restore to survive restarts). |
| Firecracker API | `--no-api --config-file <job>/vm.json`; no HTTP client, no API socket. | REST over the API socket (a dependency and a second control channel for nothing we need). |
| Jailer | **In 3b-1** (owner decision 2026-10-02). Every job VM and every inspector VM is launched through the official `jailer` v1.17.0 **without `--new-pid-ns` and without `--daemonize`**: it exec's into Firecracker in place (same pid, process group, session and stdio), so the 3a supervisor, kill, fence and `settled()` paths are untouched. Chroot under the job directory, uid/gid drop to `JAIL_UID`/`JAIL_GID` (61000), cgroup v2 `cpu.max`, `memory.max`, `memory.swap.max=0`, `pids.max`, `RLIMIT_FSIZE`, Firecracker's default seccomp filter. | `--new-pid-ns` (the jailer parent `exit(0)`s at once: the worker's child vanishes and the exit code is lost; a pid namespace adds nothing for a single-process VMM that cannot `fork`); `--daemonize` (double fork, `setsid`, stdio to `/dev/null`: breaks the process-group kill and the console capture); `--netns` (the VM has no NIC; creating a namespace needs more privilege for no gain). Both namespaces are 3b-2 if ever. |
| Jailer unavailable | The preflight **refuses** (`exit 1`, task untouched) unless `--allow-unjailed` (env `AGENTOS_ALLOW_UNJAILED=1`) is given, in which case Firecracker runs unjailed as the current user and `Submitted` records `jailed: false`. A task's `jailed` value is fixed at `submit`; later commands honour it or exit 1 before touching the task. | Silent downgrade to unjailed (the record would claim an isolation that is not there; `--worker firecracker` is a request for the sandbox); refusing always (the dev loop on the host runs as a non-root user without KVM-in-container). |
| Jail staging | Everything the VM needs is **hard-linked** into the chroot (`vmlinux`, `rootfs.squashfs` from the registry; `ws.img`, `scratch.img` from their homes; `firecracker.log` the other way round), `vm.json` is written there with chroot-relative paths, `ws.img`/`scratch.img`/`vm.json`/`firecracker.log` are `chown`ed to `JAIL_UID`. The home must be one filesystem, mounted without `nodev`/`noexec` (preflight). | Copying (≈130 MB per boot, and a copy of `ws.img` would break the reconcile argument); bind mounts from the worker (would leak mounts into the host namespace on a kill). |
| Jail leftovers | The worker removes `<job>/jail/` and `rmdir`s the cgroup after Firecracker exits. After a supervisor SIGKILL the **controller** collects them when it has fenced and settled the job (`jail::collect`), before any inspection; the inspector collects dead inspect directories of its task before creating its own. | Leaving them to a GC (hard links would keep `scratch.img` blocks alive; cgroups would accumulate). |
| KVM test tier container | `test-kvm` = `test` + `/dev/kvm` + `cap_add: [SYS_ADMIN]` + `security_opt: [seccomp=./scripts/kvm-seccomp.json, apparmor=unconfined]` + an entrypoint that remounts the container's cgroup root rw, moves the container's processes into a leaf and delegates `cpu memory pids` (measured: E11/E12). | `privileged: true` (broader, and alone it does not work: E5); `cgroupns: host` with the host's cgroup tree bind-mounted rw (jails would be created at the host root and outlive the container); `seccomp=unconfined` (works, E10, but the deny list is narrower). |
| Guest kernel | Firecracker CI `vmlinux-6.18.51`, downloaded by pinned URL and sha256 at image build. | Building a kernel (3b-2); 6.1 (shorter support). |
| Guest rootfs | Debian bookworm `mmdebstrap --variant=apt` from `snapshot.debian.org` at a pinned timestamp, plus `python3 git e2fsprogs`, packed as a **read-only squashfs** (zstd, deterministic flags). | Alpine (no snapshot archive, musl Python); ext4 rootfs (writable by construction; would need `ro` discipline). |
| Guest init | `agentos-guest`, our static musl binary, is `/sbin/init` (PID 1) and the only service. | systemd/OpenRC (boot time, surface). |
| Control channel | **vsock**, one host-initiated connection per VM, length-prefixed frames, JSON control + raw byte frames. | Serial console (no framing, shared with kernel output); a block device mailbox (polling, no backpressure); 9p/virtio-fs (not in Firecracker). |
| Workspace | A **per-task ext4 block image** `<home>/work/<task>/ws.img`, created sparse by the host, formatted and mounted only by the guest. | Host directory shared into the guest (no shared-fs device exists); re-sending the workspace every boot (no durable guest state to reconcile). |
| `reconcile` / `current_workspace` | **Inspection boot**: the same image in `inspect` mode, read-only use of `ws.img`, run by the controller, bounded, self-terminating. | Host loop-mount (needs root/`CAP_SYS_ADMIN`); parsing ext4 in the controller (a second implementation of the digest's view of the tree). |
| Handles in the guest | **No capability handle enters the guest.** The guest gets a random per-job `attempt_token` for job identity only. | Passing the real handle (a bearer secret readable by untrusted code, with nothing in the guest to present it to). |
| Resource limits | Enforced **twice**: `vcpu_count = worker_vcpus`, `mem_size_mib = worker_memory_mib`, `smt=false` in the VM config, and on the Firecracker process by the jail's cgroup (`cpu.max = worker_vcpus × 100 ms per 100 ms`, `memory.max = worker_memory_mib + 128 MiB`, `memory.swap.max = 0`, `pids.max = 64`) plus `RLIMIT_FSIZE = 1 GiB`; guest-side `RLIMIT_NPROC` and OOM priorities; bounded disks. | Balloon (`deflate_on_oom` is a courtesy, not a bound); `memory.max` without `memory.swap.max=0` (measured: the VM spills into host swap instead of being bounded, E6). |
| Worker selection | Global CLI flag `--worker host|firecracker` (env `AGENTOS_WORKER`), recorded in `Submitted`; later commands use the recorded worker. | A contract field (the contract describes the task, not the host's backend). |
| Tests without KVM | A **fake guest** (the real `agentos-guest` code running as a host process on a Unix socket that mimics Firecracker's `CONNECT` handshake) drives the whole host side; a **fake jailer** (a shell script standing in for `jailer_bin` that records its argv, creates the "cgroup" directory under a temp root and exec's the fake guest on the chroot's socket) drives the jailed launch path, staging and collection without root or KVM; KVM-gated tests run in a `test-kvm` compose service and **skip loudly** elsewhere. | Mocking the worker (would not test the protocol or the kill paths). |

## Architecture

New and changed units. Controller, journal, reducer, recovery, broker and supervisor keep their shape; the `Executor` trait is unchanged.

| Unit | Kind | Responsibility |
| --- | --- | --- |
| `agentos-core/src/workspace.rs` | moved from `agentos-engine/src/workspace.rs` (engine re-exports it, so no import changes) | The one implementation of `workspace_digest`, `copy_tree`, `list_files`, exclusions. Host and guest link the same code, so digests are identical by construction. |
| `agentos-core/src/guest.rs` | new | The control protocol: message types, framing constants, limits, `GUEST_PROTOCOL = 1`, `VSOCK_PORT = 5200`, `GUEST_CID = 3`. Pure types, serde. |
| `crates/agentos-guest` | new binary crate, built for `x86_64-unknown-linux-musl` (static) and for the host target (fake mode) | The guest init and agent: mounts, drives, uid separation, request execution, inspection queries, watchdog, shutdown. Dependencies: `agentos-core`, `serde`, `serde_json`, `rustix` (`mount`, `process`, `fs`, `net`), `vsock`. No tokio. |
| `agentos-engine/src/firecracker.rs` | new | `FirecrackerConfig`, `FirecrackerWorker` (implements `Worker`), `vm.json` rendering (host view or chroot view), process spawn (jailed or not), exit-code mapping, preflight, `Inspector`. |
| `agentos-engine/src/jail.rs` | new | The jail, as pure planning plus a few filesystem steps: `JailMode`/`JailConfig`, `JailPlan` (chroot and cgroup paths), `jailer_args` (the exact argv), `cgroup_values`, `stage` (hard links, ownership, marker file), `probe` (root, jailer, cgroup v2 writable and delegated, filesystem flags, same device), `decide` (jailed / unjailed / refuse), `collect` (cgroup `rmdir` + chroot removal), `find_cgroup2_root` (pure parser over `/proc/mounts` text). Everything but `probe`'s real environment is unit-tested over temp directories without root. |
| `agentos-engine/src/guestlink.rs` | new | Host side of the protocol: Unix-socket connect with the `CONNECT`/`OK` handshake, framing, timeouts, and the `GuestLauncher` seam (`Real` Firecracker or the fake guest). |
| `agentos-engine/src/job.rs` | changed | `WorkerConfig::Firecracker(FirecrackerConfig)`; path validation as for `HostConfig`. |
| `agentos-engine/src/supervised.rs` | changed | `reconciler` becomes `Reconciler::{Host(FixtureExecutor), Firecracker(Inspector)}`; `job_request` mints a fresh `attempt_token` per job for Firecracker configs; a settled Firecracker job's jail is collected (`jail::collect`) after `wait_for_job` and after `settled()` in the fence. |
| `agentos-cli` | changed | Global `--worker`, `--firecracker PATH`, `--jailer PATH`, `--jail-uid N`, `--jail-gid N`, `--allow-unjailed`; `agentos image register DIR \| list`; `Submitted` records the worker, image, Firecracker version, host kernel and `jailed`; `Home::executor` builds the matching executor, runs the preflight and the jail probe, and applies the jail decision. |
| `guest/python-stdlib-v1/` | new, in the repo | The image recipe: `kernel.lock` (URL + sha256), `packages.txt`, `build.sh`, `image.json` template. |
| `scripts/build-guest-image.sh`, `scripts/fetch-firecracker.sh` | new | Reproducible image build under compose; pinned Firecracker **and jailer** download into `build/firecracker/v1.17.0/` (git-ignored, never system-wide). |
| `scripts/kvm-entrypoint.sh`, `scripts/kvm-seccomp.json` | new | The two files that make the jailer work inside the `test-kvm` container: the cgroup delegation entrypoint (remount rw, move the container's pids into `init/`, `+cpu +memory +pids`) and the deny-list seccomp profile (`defaultAction: SCMP_ACT_ALLOW`; 49 syscalls denied with `EPERM`: host-dangerous calls, a subset of what Docker's default profile gates (behind a capability, a kernel version or outright; Docker allows `kcmp` and `process_vm_*` on kernels ≥ 4.8, for one) — module loading, `kexec`, `reboot`, `swapon`, `open_by_handle_at`, `bpf`, `perf_event_open`, `ptrace`-adjacent `process_vm_*`, `userfaultfd`, `setns`, time setting, keyring, `quotactl`, `io_uring_*`, … — none of which the jailer or Firecracker use; `pivot_root` is allowed). |
| `compose.yaml`, `Dockerfile` | changed | `test-kvm` service: `extends: test`, `devices: ["/dev/kvm:/dev/kvm"]`, `cap_add: [SYS_ADMIN]`, `security_opt: ["seccomp=./scripts/kvm-seccomp.json", "apparmor=unconfined"]`, `entrypoint: ["sh", "scripts/kvm-entrypoint.sh"]`, `AGENTOS_KVM_TESTS=1`, `AGENTOS_FIRECRACKER`, `AGENTOS_JAILER`, `AGENTOS_GUEST_IMAGE`, the `guest-images` volume; the `test` service is unchanged. Image gains `mmdebstrap squashfs-tools musl-tools` and the musl target. |

### Process model

```
controller (agentos CLI)                        [holds driver.lock]
  └─ agentos supervise run <job>  (session leader, subreaper, holds <job>/lock via stdin)
       └─ agentos supervise worker <job>  (own process group = worker_pgid)
            └─ jailer --id <attempt> … -- --no-api --config-file /vm.json      (same group; root)
               ═exec═▶ /firecracker --id <attempt> … --no-api --config-file /vm.json
                       (same pid; uid 61000; chroot <job>/jail/firecracker/<attempt>/root;
                        cgroup <cgroup root>/agentos/<attempt>; stdout → console.log)
                 └─ guest: agentos-guest (PID 1) ─┬─ git apply       as uid 1000 (builder)
                                                 └─ check command   as uid 1001 (check)
```

Firecracker is a plain child of the worker in the worker's process group: the worker spawns the **jailer**, which sets up the chroot, cgroup and limits and then `exec`s Firecracker **in the same process** (pid, pgid, sid and the three stdio fds unchanged; measured in E10/E11). So every 3a kill path reaches it unchanged: the supervisor's `kill_process_group(worker_pgid)` on lease, deadline or cancel; `reap_all` as subreaper; the controller's `kill_job`/`kill_session` fence after a supervisor SIGKILL (the session scan finds the jailed process by its session id, which it kept); `settled()` refusing to settle while anything of the job's session lives; and the worker's `Child::wait` returns Firecracker's own exit status. The chroot, the uid drop and the cgroup change nothing about signals: a root worker signals a uid-61000 member of its own group as before, and a SIGKILLed Firecracker process destroys its VM instantly (the KVM VM is a kernel object owned by the process). Nothing in the guest can outlive the Firecracker process. Unjailed (`--allow-unjailed`), the worker spawns `firecracker` directly with the same arguments against host paths; the rest is identical.

Belt and braces: the guest agent **shuts the VM down when its control connection closes** (EOF on the vsock connection → `sync`, unmount, `reboot(RB_AUTOBOOT)`; with `reboot=k` Firecracker exits 0). So a worker that dies for any reason takes its VM down within milliseconds even before the supervisor's kill lands, and an inspector VM dies with the controller that opened it.

### Home layout additions

```text
<home>/bin/firecracker                   optional: the pinned Firecracker binary (see --firecracker)
<home>/bin/jailer                        optional: the pinned jailer (see --jailer; default: `jailer` next to the Firecracker binary)
<home>/registry/images/<id>@<digest>/    registered guest images, read-only (root 0444): image.json, vmlinux, rootfs.squashfs
<home>/registry/images/<id>@<digest>.meta.json
<home>/work/<task>/ws.img                the task workspace block image (sparse ext4, WS_IMAGE_BYTES); owned by JAIL_UID when jailed
<home>/work/<task>/ws.lock               advisory lock: who has ws.img attached
<home>/jobs/<effect>-<attempt>/vm.json   Firecracker configuration — unjailed only (jailed: inside the chroot)
<home>/jobs/<effect>-<attempt>/v.sock    vsock host socket — unjailed only (jailed: <chroot>/v.sock)
<home>/jobs/<effect>-<attempt>/scratch.img   per-job scratch drive (sparse, SCRATCH_IMAGE_BYTES)
<home>/jobs/<effect>-<attempt>/console.log   guest serial console (kernel + agent log): Firecracker's stdout
<home>/jobs/<effect>-<attempt>/firecracker.log   Firecracker's own log (jailed: a hard link of <chroot>/firecracker.log)
<home>/jobs/<effect>-<attempt>/stderr.log    stderr of the spawned process (the jailer's error text, if any)
<home>/jobs/<effect>-<attempt>/jail/cgroup   marker: the absolute cgroup path of this job's VM (one line; for collection)
<home>/jobs/<effect>-<attempt>/jail/firecracker/<attempt>/root/   the chroot ("The jail" below)
<home>/inspect/<task>/<uuid>/            inspector VMs: scratch.img, console.log, firecracker.log, stderr.log, jail/ (removed on success)
```

The job-directory single-writer rule extends: `vm.json`, `scratch.img`, `console.log`, `firecracker.log`, `stderr.log` and the `jail/` tree belong to the worker (Firecracker and the jailer write into them on the worker's behalf), `v.sock` to Firecracker. `JobDir::list` ignores the directory `jail` in addition to nothing else: these names never collide with the protocol files. The controller touches `jail/` only to collect it after the job is settled.

### Worker configuration

```rust
pub enum WorkerConfig { Host(HostConfig), Scripted(ScriptedConfig), Firecracker(FirecrackerConfig) }

pub struct FirecrackerConfig {
    pub firecracker_bin: PathBuf,      // absolute
    pub image_dir: PathBuf,            // absolute: <registry>/images/<id>@<digest>/
    pub image_digest: Digest,          // pinned; re-digested before every launch
    pub snapshot_dir: PathBuf,         // absolute, as HostConfig
    pub profile_dir: PathBuf,          // absolute, as HostConfig
    pub profile_digest: Option<Digest>,
    pub work_root: PathBuf,            // absolute; ws.img lives at <work_root>/<task>/ws.img
    pub verify_timeout_secs: u64,      // 60, as today
    pub vcpus: u32,                    // contract worker_vcpus, 1..=32
    pub memory_mib: u32,               // contract worker_memory_mib, >= GUEST_MIN_MEMORY_MIB (128)
    pub attempt_token: String,         // 32 lowercase hex, minted per job by SupervisedExecutor::job_request
    pub launcher: GuestLauncher,       // Real, or Fake { guest_bin } (honoured only with AGENTOS_TEST_WORKERS=1)
    pub jail: JailMode,                // Jailed(JailConfig) or Unjailed; decided by the controller, never by the worker
}

pub enum JailMode { Jailed(JailConfig), Unjailed }

pub struct JailConfig {
    pub jailer_bin: PathBuf,           // absolute; `--version` must print `Jailer v1.17.`
    pub uid: u32,                      // JAIL_UID = 61000 unless --jail-uid
    pub gid: u32,                      // JAIL_GID = 61000 unless --jail-gid
    pub cgroup_root: PathBuf,          // the cgroup v2 mount point found by the probe (`/sys/fs/cgroup`)
}
```

`JobDir::create` and `run_worker` reject relative paths exactly as for `HostConfig` (`jailer_bin` and `cgroup_root` included). `vcpus` and `memory_mib` are validated at `submit` for the Firecracker worker (`worker_vcpus <= 32`, `worker_memory_mib >= 128`, exit 2 with the limit named); the contract schema itself does not change.

Selection: `agentos --worker firecracker submit …` (or `AGENTOS_WORKER=firecracker`). `submit` resolves the guest image from `contract.profile` (the build plan's repository profile, e.g. `python-stdlib-v1`) exactly as `resolve_profile` resolves verification profiles: an optional contract pin `guest_image_digest` (`#[serde(default, skip_serializing_if = "Option::is_none")]`, 64 lowercase hex, so existing contract digests do not change) ⇒ exactly that registry entry else exit 2; else the newest registry entry for the id; no legacy directory fallback. The `Submitted` payload gains `worker` (`"host"` or `"firecracker"`) and, for Firecracker, `guest_image_id`, `guest_image_digest`, `firecracker_version`, `host_kernel` (from `uname -r`, for attribution; owner decision 7) and `jailed` (`true` | `false`). `resume`, `cancel`, `revoke`, `status` and `export` build the executor from the recorded worker; `--worker` is optional: absent, `submit` uses `host` and every other command uses the record; present and different from the record it exits 2 (`task was submitted with worker host`). The recorded `jailed` is honoured the same way: a task submitted jailed is always run jailed (if the probe fails later, the command exits 1 `task was submitted jailed: jailer unavailable: …` before touching the task — the same class as a vanished `/dev/kvm`), and a task submitted unjailed stays unjailed without needing the flag again (its `ws.img` is owned by the submitting user, and the acknowledgement was given at `submit`). The `profile` field is ignored by the host worker as today.

`--firecracker PATH` (env `AGENTOS_FIRECRACKER`) names the binary; default `<home>/bin/firecracker`. `--jailer PATH` (env `AGENTOS_JAILER`) names the jailer; default: the file `jailer` next to the Firecracker binary. `--jail-uid N` / `--jail-gid N` (env `AGENTOS_JAIL_UID` / `AGENTOS_JAIL_GID`) default to `JAIL_UID = JAIL_GID = 61000` (an id in the range Debian reserves and never allocates; no user needs to exist for the jailer's `setuid`). `--allow-unjailed` (env `AGENTOS_ALLOW_UNJAILED=1`) is the explicit fallback (below). `scripts/fetch-firecracker.sh [DEST]` downloads the v1.17.0 tarball, verifies the tarball, binary and jailer sha256s above and installs `firecracker` and `jailer` into `DEST` (default `build/firecracker/v1.17.0/`). Nothing is installed outside the repo or the home.

### Preflight

`Home::executor` for a Firecracker task runs `FirecrackerWorker::preflight(&config)` before anything is journaled: `/dev/kvm` opens read-write; `firecracker_bin` is executable and `--version` prints `Firecracker v1.17.`; `image_dir/image.json` parses, has `protocol == GUEST_PROTOCOL` and names existing `vmlinux` and `rootfs.squashfs`; `workspace_digest(image_dir) == image_digest`. A failure exits the command with code 1 and the reason (`firecracker worker unavailable: /dev/kvm: Permission denied`) **before** the task is touched, so a missing KVM never fails a task. The worker repeats the same checks before every launch (defense in depth; a failure there is `Failure("firecracker worker unavailable: …")`, journaled like any effect failure).

**Jail probe and decision** (`jail::probe`, `jail::decide`; after the checks above, `Real` launcher only — the `Fake` launcher is never jailed and records `jailed: false`, except under the CLI test hook `AGENTOS_TEST_JAIL_PROBE`, which replaces the probe's answer so the decision and the record can be tested without root): the probe succeeds when all of these hold, and otherwise names the first failure:

1. `geteuid() == 0` (`needs root (euid 0), running as uid N`);
2. `jailer_bin` is executable and `--version` prints `Jailer v1.17.` (`jailer --version: …`);
3. `/proc/mounts` has a `cgroup2` mount (`find_cgroup2_root`; `no cgroup v2 hierarchy in /proc/mounts`), whose `cgroup.controllers` lists `cpu`, `memory` and `pids` (`controllers missing in <root>/cgroup.controllers: <list>`);
4. `<root>/agentos` can be created and `+cpu +memory +pids` can be written into `<root>/cgroup.subtree_control` and `<root>/agentos/cgroup.subtree_control` — the writes the jailer will make (`EROFS` ⇒ `cgroup v2 hierarchy <root> is read-only`; `EBUSY` ⇒ `cannot delegate cpu, memory, pids in <root>: Device or resource busy (the root cgroup has processes of its own; scripts/kvm-entrypoint.sh shows the delegation)`; other errors verbatim);
5. the filesystem holding `<home>/jobs` (and `<home>/inspect`) is mounted without `nodev` and without `noexec` (`statvfs` flags; `jail base <path> is on a nodev filesystem` / `… noexec …`), and `<home>/jobs`, `<home>/work` and the registry entry are on the same device (`st_dev`; `<a> and <b> are on different filesystems: the jail hard-links them`).

Decision: probe ok ⇒ `JailMode::Jailed`. Probe failed and `--allow-unjailed` absent ⇒ **the command exits 1** before the task is touched: `firecracker worker unavailable: jailer unavailable: <reason>; pass --allow-unjailed to run Firecracker without a jail as the current user`. Probe failed and `--allow-unjailed` present ⇒ `JailMode::Unjailed`, `Submitted.jailed = false`, and the command prints one line on stderr (`warning: running Firecracker unjailed: <reason>`). Refusing is the default because `--worker firecracker` is a request for the sandbox and the record must not claim what the host could not provide; the flag keeps the non-root developer loop possible and leaves the fact in the journal. The probe is deterministic per environment, which the tests rely on: in the `test` compose service it fails at step 4 with `read-only`; on the host as the dev user it fails at step 1; in `test-kvm` it succeeds.

### Guest image

Registry entry `<home>/registry/images/<id>@<digest>/`:

```text
image.json        {"id":"python-stdlib-v1","protocol":1,"kernel":"vmlinux","rootfs":"rootfs.squashfs",
                   "agent_version":"0.1.0","kernel_sha256":"0545ba…","built_from":"guest/python-stdlib-v1@<git-sha>"}
vmlinux           Firecracker CI vmlinux-6.18.51 (27,882,928 bytes)
rootfs.squashfs   read-only root, zstd
```

`digest` = `workspace_digest` over the three files (same rules as profiles; they are regular files). `agentos image register DIR` copies, makes read-only, writes `.meta.json`, dedupes by bytes, refuses ids containing `@`, `/` or traversal, prints `{"id","digest"}`; `agentos image list` lists entries. Both mirror `profile register|list` and share its code.

`scripts/build-guest-image.sh guest/python-stdlib-v1 OUT_DIR` (run as `docker compose run --rm test sh scripts/build-guest-image.sh …`):

1. `curl` the kernel from `kernel.lock` (`url`, `sha256`), verify, copy as `vmlinux`.
2. `cargo build --release -p agentos-guest --target x86_64-unknown-linux-musl`.
3. `mmdebstrap --mode=root --variant=apt --include=$(cat packages.txt) bookworm "$ROOT" "https://snapshot.debian.org/archive/debian/20260901T000000Z/"` with `SOURCE_DATE_EPOCH=1756684800`, then hooks that: install the agent at `/sbin/agentos-guest` and symlink `/sbin/init` to it; create users `builder` (1000) and `check` (1001) with no shell login; create `/workspace`, `/scratch`, `/run`, `/tmp`; remove `/var/cache/apt`, `/var/lib/apt/lists/*`, `/var/log/*`, `/etc/machine-id`, `/usr/share/doc`, `/usr/share/man`, locales; set `/etc/hostname` to `agentos-guest`; leave no resolv.conf, no network configuration. `packages.txt`: `python3-minimal libpython3-stdlib git e2fsprogs` (the fixture profile needs `python3`; patches need `git apply`; `mkfs.ext4` formats the drives).
4. `mksquashfs "$ROOT" rootfs.squashfs -comp zstd -all-root -no-xattrs -mkfs-time 0 -all-time 0 -noappend -no-progress -no-recovery`.
5. Write `image.json`; print the digest. `--verify` builds twice into two directories and fails unless the digests are equal (the reproducibility check, run in the KVM tier and before any registration in `scripts/demo.sh`).

`Dockerfile` adds `mmdebstrap squashfs-tools` and `rustup target add x86_64-unknown-linux-musl`. The build needs root inside the container (compose gives it) and network access to snapshot.debian.org and the CI bucket; the result is cached in the compose volume `guest-images` so tests do not rebuild it.

Inside the guest at boot (`agentos-guest` as PID 1):

| Mount / device | Purpose | Who can write |
| --- | --- | --- |
| `/` = `/dev/vda` (squashfs, `is_read_only: true`) | the image | nobody |
| `/proc`, `/sys`, `/dev` (devtmpfs), `/run` and `/tmp` (tmpfs, `size=64m` each) | runtime | agent; `/tmp` also `check` |
| `/workspace` = `/dev/vdb` (`ws.img`, ext4, `cache_type: Writeback`) | the task workspace, mounted `rw` for jobs, `rw` then `remount,ro` for inspection | `builder` (owner, 0755); the check reads only |
| `/scratch` = `/dev/vdc` (`scratch.img`, ext4 formatted at boot, `cache_type: Unsafe`) | staged profile (`/scratch/profile`), bytecode cache, reconcile scratch copy | agent, `check` (its own subdirectory `/scratch/check`, 0700) |
| `/dev/vsock` | control channel | root only (0600): the check cannot talk to the host |

No network interface exists (`network-interfaces: []`); only `lo` is up. The kernel command line is `console=ttyS0 reboot=k panic=1 pci=off nomodule quiet loglevel=4 init=/sbin/agentos-guest` plus what Firecracker appends (`root=/dev/vda ro`, `virtio_mmio.device=…`); it carries nothing secret. Firecracker's stdout (the serial console) goes to `<job>/console.log`; the check's own output is captured by the agent, never written to the console (ttyS0 is root-only), so `console.log` is bounded by kernel and agent chatter.

### Workspace image and scratch

- `ws.img` is created by the worker on `ReadSnapshot`: `File::create` + `set_len(WS_IMAGE_BYTES)` (sparse; **1 GiB**), re-created from zero on every `ReadSnapshot` attempt (so a retry starts clean, as `read_snapshot` removes the directory today). The guest formats it (`mkfs.ext4 -q -F -E lazy_itable_init=0,lazy_journal_init=0 /dev/vdb`), mounts it, writes the snapshot files, `syncfs`, digests. `ApplyPatch` and `RunVerification` require it to exist (`workspace missing: no snapshot was read`, the 3a wording).
- `scratch.img`: created by the worker in the job directory (sparse, **512 MiB**), formatted by the guest at boot, removed by the worker after `outcome.json` is written. A killed job leaves it behind with its directory (3a's known limit "no job-directory GC"; 3b-2 collects).
- `ws.lock` (`<work>/<task>/ws.lock`): the worker takes an exclusive `flock` before spawning Firecracker and holds it until Firecracker has exited; the inspector does the same. The lock is a guard against a programming error, never the mechanism: 3a already guarantees that no two attempts of a task overlap (one driver per home, `wait_for_job`/`fence_job`/`settled` before any reconcile or redispatch). `try_lock` failure is `Failure("workspace image is attached to another VM")` for `Retry` kinds and, for `ApplyPatch`, an **unresolved** outcome (never a plain failure, since the other VM may be applying the patch).
- Durability discipline: `Writeback` cache mode for `ws.img` and an explicit `syncfs(/workspace)` in the guest before any `PatchApplied`/`SnapshotDone` reply, so **a reported success is durable in `ws.img` before the host can write an outcome**. This is what makes the reconcile trichotomy sound after a VM death: the image is either the base, the base plus this patch (possibly with a journal to replay), or something else.
- Host disk filling is bounded per task to `WS_IMAGE_BYTES + SCRATCH_IMAGE_BYTES` of real blocks plus two 64 MiB tmpfs that are guest memory, not host disk.

### The jail

Every job VM and every inspector VM runs through the jailer when `JailMode::Jailed`. The worker (or the inspector) does, in order:

1. **Plan** (`jail::plan`): `id` = the attempt id (job) or `inspect-<uuid>` (inspector), validated against the jailer's rule (`[A-Za-z0-9-]{1,64}`); `exec_name` = the file name of `firecracker_bin`; `base` = `<dir>/jail` where `<dir>` is the job directory or the inspect directory; `chroot` = `<base>/<exec_name>/<id>/root`; `cgroup` = `<cgroup_root>/agentos/<id>`.
2. **Stage** (`jail::stage`): `mkdir -p <chroot>`; hard-link `<image_dir>/vmlinux` → `<chroot>/vmlinux` and `<image_dir>/rootfs.squashfs` → `<chroot>/rootfs.squashfs` (registry files are root-owned `0444`: readable by the jail uid, not writable, and the hard link exposes the inode, never the registry path); hard-link `<work_root>/<task>/ws.img` → `<chroot>/ws.img` and `<dir>/scratch.img` → `<chroot>/scratch.img`, then `chown JAIL_UID:JAIL_GID` and `chmod 0600` both (a hard link shares the inode, so the files at their original paths are now owned by the jail uid too — "ws.img ownership" below); create `<chroot>/firecracker.log` (empty, `0600`, jail uid) and hard-link it to `<dir>/firecracker.log`; write `<chroot>/vm.json` (chroot view, `0644`, jail uid); write the marker `<dir>/jail/cgroup` containing the absolute cgroup path. `EXDEV` on any link is `cannot prepare the jail: <a> and <b> are on different filesystems` (the preflight checks this first). `<chroot>/<exec_name>` must **not** exist: the jailer copies the binary itself and refuses a hard link there.
3. **Spawn** the jailer (exact argv; `vcpus` and `memory_mib` from the config, `id` from the plan):

   ```text
   <jailer_bin> --id <id> --exec-file <firecracker_bin> --uid <JAIL_UID> --gid <JAIL_GID>
       --chroot-base-dir <dir>/jail --cgroup-version 2 --parent-cgroup agentos
       --cgroup "cpu.max=<vcpus × 100000> 100000" --cgroup memory.max=<(memory_mib + 128) × 1048576>
       --cgroup memory.swap.max=0 --cgroup pids.max=64
       --resource-limit fsize=1073741824
       -- --no-api --config-file /vm.json
   ```

   with `env_clear()`, cwd `<dir>`, stdin null, stdout `<dir>/console.log`, stderr `<dir>/stderr.log`, and the job's process group (no `process_group(0)` for the worker; `process_group(0)` for the inspector, which is the controller's own child). No `--new-pid-ns`, no `--daemonize`, no `--netns`, no `no-file` (the jailer's default 2048 is ample: Firecracker opens the KVM fds, three drives, the socket and its log). The jailer exec's Firecracker in place; `Child::wait` yields Firecracker's exit status, exactly as unjailed. The worker's `GuestLink::connect` waits for `<chroot>/v.sock` instead of `<job>/v.sock`; everything after that is the same code path.
4. **Collect** (`jail::collect(<dir>, <cgroup_root>)`), after Firecracker has exited: `rmdir` the cgroup named by the marker — which must lie under `<cgroup_root>/agentos/`, else the marker is refused and nothing is removed (a worker-writable file never directs the controller outside the jail's cgroup subtree) — (`ENOENT` is fine; `EBUSY`/`ENOTEMPTY` means something still runs — impossible after `wait`, reported as a warning and left for the controller), then `remove_dir_all(<dir>/jail)` (the chroot holds the jailer's binary copy, `vm.json`, `v.sock`, `firecracker.pid`, `dev/`, `run/`, the hard links; root can remove the jail uid's `0700` directories). `<dir>/firecracker.log` keeps its link count 1 and stays readable in the job directory.

**Why the limits are what they are.** `cpu.max = vcpus × 100 ms per 100 ms` gives the VMM thread and the vCPU threads together as many CPUs as the contract promised the guest; a saturating 1-vCPU guest was throttled 1 ms in 3 s under it (E7b) and halved under a 50 % quota (E7), so the bound is real without costing the honest guest anything. `memory.max = memory_mib + 128 MiB`: Firecracker's own footprint above guest memory measured 45–60 MB at full guest occupancy (E6b/E6d), the KVM tier asserts `VmRSS ≤ mem_size_mib + 96 MiB`, and the cgroup bound sits above both, so the host OOM killer only fires on a VMM that leaks or is compromised. `memory.swap.max = 0` because without it the bound is not a bound (E6: the VM spilled into swap). `pids.max = 64`: Firecracker runs one thread per vCPU (≤ 32) plus a handful; its seccomp filter forbids `fork`/`exec`, so the limit only ever bites a compromised VMM. `RLIMIT_FSIZE = 1 GiB = WS_IMAGE_BYTES`: the largest file Firecracker legitimately writes is `ws.img` (writes within an existing 1 GiB file never exceed the limit); `console.log` and `firecracker.log` are bounded by the same number; exceeding it is `SIGXFSZ`, Firecracker exit code 151, mapped like any other exit (`guest exited before reporting: firecracker exit code 151`).

**ws.img ownership and the controller's inspection.** Jailed, `ws.img` belongs to `JAIL_UID` from the first `ReadSnapshot` on. The controller runs as root in that mode (the probe guarantees it), so it can `stat` and open the image for the "missing" check and for `ws.lock`; the inspector boots through the same jailer with the same uid, so the inspection VM reads and journal-replays an image it owns. `scratch.img` is per job and removed with it. Nothing else of the home changes owner: the job directory, `request.json`, `outcome.json`, the journal and the registry stay the controller's. A home that was used jailed and is later opened by a non-root user cannot read its `ws.img`s — that is the "submitted jailed" rule above (exit 1, task untouched), not a silent failure.

**Leftovers after a supervisor SIGKILL.** The worker died with its supervisor, so step 4 never ran. What remains is inert: the cgroup `agentos/<attempt>` with no process in it (the fence killed Firecracker; E8) and the chroot directory with its hard links (which keep `scratch.img`'s blocks alive until removed). The **controller** collects both: `SupervisedExecutor` calls `jail::collect(job)` for a Firecracker job in the two places where 3a has proven the job settled — after `wait_for_job` in the normal path and after `settled()` in `fence_jobs` on resume — and the inspector calls it for every `<home>/inspect/<task>/<uuid>/` it finds before creating its own (only one controller runs per home, so any inspect directory found is a dead inspector's). Collection never precedes settlement: `rmdir` of a cgroup with a live process fails, and a chroot under a live VM must not be touched. The job directory itself is still not garbage-collected (3a known limit), but it no longer holds anything of size.

**Unjailed fallback.** `JailMode::Unjailed` (only through `--allow-unjailed` or a `jailed: false` record) skips steps 1–4: the worker spawns `firecracker` directly against `<job>/vm.json` with host paths, `v.sock` and the log in the job directory, `ws.img` owned by the current user, no cgroup. The VM's own limits (`vcpu_count`, `mem_size_mib`, no NIC, drives) and Firecracker's default seccomp filter are unchanged. The README lists it as "unjailed Firecracker: the controller's UID is the blast radius".

**Compose settings for the KVM tier** (verified E11/E12; the only settings under which the jailer works in a container, each as narrow as the experiments allowed):

```yaml
  test-kvm:
    extends: test
    devices: ["/dev/kvm:/dev/kvm"]              # the VM
    cap_add: [SYS_ADMIN]                        # jailer: unshare(CLONE_NEWNS), mount, pivot_root; entrypoint: remount cgroup rw
    security_opt:
      - seccomp=./scripts/kvm-seccomp.json      # Docker's default profile denies pivot_root for every container
      - apparmor=unconfined                     # Docker's default AppArmor profile denies mount where AppArmor is on; a no-op here
    entrypoint: ["sh", "scripts/kvm-entrypoint.sh"]   # cgroup v2 delegation inside the container's own cgroup
    environment: [AGENTOS_KVM_TESTS=1, AGENTOS_FIRECRACKER=/work/build/firecracker/v1.17.0/firecracker,
                  AGENTOS_JAILER=/work/build/firecracker/v1.17.0/jailer, AGENTOS_GUEST_IMAGE=/work/build/guest-images/python-stdlib-v1]
    volumes: ["guest-images:/work/build/guest-images"]
```

`scripts/kvm-entrypoint.sh`: `mount -o remount,rw /sys/fs/cgroup`; `mkdir -p /sys/fs/cgroup/init`; move every pid of `/sys/fs/cgroup/cgroup.procs` (tini, the shell) into `init/cgroup.procs`; write `+cpu +memory +pids` to `/sys/fs/cgroup/cgroup.subtree_control`; `exec "$@"`. The cgroup namespace stays private, so the jailer's cgroups live under the container's own cgroup and die with the container; the host's cgroup tree, its namespaces and its devices are never touched; the seccomp profile keeps every host-dangerous syscall denied. `privileged: true` is not used (and alone does not work: E5).

### Firecracker configuration (`<job>/vm.json`, written by the worker)

```json
{
  "boot-source": { "kernel_image_path": "<image_dir>/vmlinux",
                   "boot_args": "console=ttyS0 reboot=k panic=1 pci=off nomodule quiet loglevel=4 init=/sbin/agentos-guest" },
  "drives": [
    { "drive_id": "rootfs",    "is_root_device": true,  "is_read_only": true,  "path_on_host": "<image_dir>/rootfs.squashfs", "cache_type": "Unsafe" },
    { "drive_id": "workspace", "is_root_device": false, "is_read_only": false, "path_on_host": "<work_root>/<task>/ws.img",   "cache_type": "Writeback" },
    { "drive_id": "scratch",   "is_root_device": false, "is_read_only": false, "path_on_host": "<job>/scratch.img",            "cache_type": "Unsafe" }
  ],
  "machine-config": { "vcpu_count": <worker_vcpus>, "mem_size_mib": <worker_memory_mib>, "smt": false, "huge_pages": "None" },
  "vsock": { "guest_cid": 3, "uds_path": "<job>/v.sock" },
  "network-interfaces": [],
  "logger": { "log_path": "<job>/firecracker.log", "level": "Warning" }
}
```

No `balloon`, no `mmds-config`, no `entropy` beyond the kernel's own, no `cpu-config`. The document above is the **unjailed** rendering (host paths; written to `<job>/vm.json`). The **jailed** rendering is the same document with every path replaced by its chroot-relative name — `/vmlinux`, `/rootfs.squashfs`, `/ws.img`, `/scratch.img`, `/v.sock`, `/firecracker.log` — written to `<chroot>/vm.json`. One renderer, two `VmView`s; both are golden-tested.

Spawn, unjailed: `<firecracker_bin> --no-api --config-file <job>/vm.json --id <attempt_id>`. Spawn, jailed: the argv in "The jail" (the jailer adds `--id` itself). Both: `env_clear()`, cwd `<job>`, stdin null, stdout `console.log`, stderr `stderr.log`, in the worker's process group (no `process_group(0)`: it must stay in `worker_pgid`). Drive order fixes the guest names `vda`, `vdb`, `vdc`. The inspector's `vm.json` is identical except that `scratch.img`, the socket and the log live under `<home>/inspect/<task>/<uuid>/` (or its chroot) and the id is `inspect-<uuid>`; `workspace` is still `<work_root>/<task>/ws.img` (read-write for journal replay; the agent remounts it read-only before answering).

### Guest control protocol (vsock port 5200)

One host-initiated connection per VM. Host side: wait for `<job>/v.sock` to exist (poll every 10 ms, at most `BOOT_TIMEOUT` = 15 s from spawn), connect, write `CONNECT 5200\n`, read a line; `OK <n>\n` means the guest is listening; a closed connection means not yet (reconnect every 50 ms until `BOOT_TIMEOUT`). Then frames.

Frame: `u32` big-endian body length, `u8` kind (`0` = JSON, `1` = raw bytes), body. Limits (constants in `agentos_core::guest`): JSON frame ≤ 1 MiB; raw frame ≤ 16 MiB; one file ≤ 64 MiB; a snapshot ≤ 256 MiB and ≤ 65 536 files; a profile ≤ 64 MiB; a patch ≤ 4 MiB; captured stdout/stderr ≤ 64 KiB + 1 (the existing `OUTPUT_LIMIT`, truncation flags preserved). A frame over its limit closes the connection: on the host that is a protocol failure (`Failure("guest protocol violation: …")`, or for `ApplyPatch` an unresolved outcome unless the request was never sent); in the guest it is a shutdown.

Messages (JSON, `{"type": "...", ...}`; `b64` fields carry bytes inline, files follow as raw frames):

| Direction | Type | Fields |
| --- | --- | --- |
| H→G | `Hello` | `protocol`, `attempt_token`, `task_id`, `effect_id`, `attempt_id`, `lease_generation`, `mode` (`job` \| `inspect`) |
| G→H | `Ready` | `protocol`, `agent_version`, `mode`, `vcpus` (as the guest sees them), `memory_mib` |
| H→G | `ReadSnapshot` | `file_count`, `total_bytes`; then `file_count` × (`File{path, len}` + one raw frame); then `EndFiles` |
| G→H | `SnapshotDone` | `files` (sorted relative paths), `workspace_digest` |
| H→G | `ApplyPatch` | `expected_base`, `editable_paths`; then one raw frame (the patch) |
| G→H | `PatchApplied` | `paths` (patch order, from `git apply --numstat`), `workspace_digest` |
| H→G | `RunVerification` | `profile_digest` (pin, optional), `timeout_secs`, `file_count`, `total_bytes`; files; `EndFiles` |
| G→H | `Verified` | `profile_id`, `command`, `profile_digest`, `workspace_digest`, `exit_code` (int or null), `stdout_b64`, `stdout_truncated`, `stderr_b64`, `stderr_truncated` |
| H→G | `Digest` | — (inspect mode) |
| G→H | `DigestIs` | `workspace_digest` |
| H→G | `PatchState` | `expected_base`; then one raw frame (the patch) (inspect mode) |
| G→H | `PatchStateIs` | `state` (`not_applied` \| `applied` \| `unknown`), `paths`, `workspace_digest`, `reason` |
| G→H | `Refused` | `reason` — any validation failure, with the 3a wording (below) |
| H→G | `Shutdown` | — |
| G→H | `Bye` | — |

Rules: the first frame must be `Hello` with `protocol == 1` and a token the guest has not seen bound to another attempt (a second `Hello` with a different token closes the connection); `mode` fixes which requests are accepted (`job`: `ReadSnapshot`, `ApplyPatch`, `RunVerification`, `Shutdown`; `inspect`: `Digest`, `PatchState`, `Shutdown`). One request at a time, one reply each. The host builds **every outcome byte on the host** from the structured reply (`ExecOutcome::success` with exactly the JSON the fixture executor builds today: `{"files","workspace_digest"}`, `{"applied":true,"paths","workspace_digest"}`, and the evidence object with `summary, profile_id, profile_digest, workspace_digest, command, exit_code, passed, stdout, stdout_truncated, stderr, stderr_truncated`), so artifacts, manifests and bundles are byte-compatible with the host worker's. The receipt (effect id, attempt id, lease generation, result digest) is also built on the host from `request.json`; the guest never sees or produces a receipt.

`Refused.reason` strings are the 3a strings, so observations and tests do not change: `workspace missing: no snapshot was read`, `invalid patch: …`, `unsupported patch operation: …`, `binary patches are not supported: …`, `patch touches no files`, `path not editable: <p>`, `path excluded from the workspace digest: <p>`, `path <p> crosses symlink <l>`, `version conflict: expected <b>, actual <a>`, `patch does not apply: …`, `profile and workspace overlap` (unreachable in the guest, kept for parity), `profile digest mismatch: pinned <d>, found <x>`, `invalid profile.json: …`, `profile command is empty`, `timeout`, `cannot run profile command: …`, `workspace changed during verification`, `workspace polluted by excluded entries: …`.

### Effects inside the guest

- `ReadSnapshot`: zero and format `/dev/vdb`, mount `/workspace`, write the files as `builder` (0644/0755 directories; mode bits are outside the digest), `syncfs`, `workspace_digest("/workspace")`, reply. The host asserts nothing beyond the protocol; the journal's `WorkspaceUpdated` carries the guest's digest, and the CLI's existing `check_inputs` already ties the staged snapshot to the submitted digest.
- `ApplyPatch`: as `FixtureExecutor::try_apply_patch`, in the same order: parse (`git apply --numstat -z`, `--summary` with the same allowed summary lines), editable paths against `editable_paths` (the contract rule `path_matches`, shared code), excluded components, symlinks on the path, `purge_excluded`, digest equals `expected_base` else version conflict, `git apply --check`, `git apply` (as `builder`, scrubbed environment, `GIT_CEILING_DIRECTORIES=/`), `syncfs`, digest, reply.
- `RunVerification`: stage the profile under `/scratch/profile` (fresh per run, outside the workspace), digest it, compare with the pin (`profile digest mismatch`) — unpinned runs compare the staged digest with itself after the run, as today; parse `profile.json` from the staged bytes; `purge_excluded(/workspace)`; digest the workspace; run `command… /workspace` as `check` with `env_clear`, `PATH=/usr/bin:/bin`, `PYTHONDONTWRITEBYTECODE=1`, `PYTHONPYCACHEPREFIX=/scratch/check/pycache`, cwd `/scratch/profile`, in its own process group, `RLIMIT_NPROC=256`, `RLIMIT_NOFILE=1024`, `oom_score_adj=1000` (the agent runs at `-1000`), stdout/stderr captured to 64 KiB + 1; timeout `timeout_secs` ⇒ kill the group ⇒ `Refused("timeout")`; afterwards re-digest the staged profile and the workspace (`workspace changed during verification` cannot happen through the check, which cannot write `/workspace`, but the check is re-run through the same code and the check stays), `excluded_entries` ⇒ `workspace polluted …`; reply `Verified`. A check killed by the guest OOM killer reports `exit_code: null` (as a signal-killed process does today).
- `Digest` (inspect): `workspace_digest("/workspace")` after mount `rw` → `remount,ro` (journal replay, nothing else).
- `PatchState` (inspect): `FixtureExecutor::patch_state` ported: digest equals base ⇒ `not_applied`; else copy `/workspace` to `/scratch/reverse`, `git apply --reverse`, digest equals base ⇒ `applied` with the patch's paths and the current digest; else `unknown` with the 3a reason text (`workspace <a> is neither the base <b> nor the base with this patch`).
- `Shutdown`: `syncfs`, unmount `/workspace` and `/scratch`, `reboot(RB_AUTOBOOT)`.
- Watchdog: no `Hello` within 10 s of boot, or EOF/any error on the connection ⇒ `Shutdown` path.

### Inspection (`reconcile`, `current_workspace`)

`SupervisedExecutor` with a Firecracker config holds `Reconciler::Firecracker(Inspector)`. `Inspector::query(task, Query::Digest | Query::PatchState{expected_base, patch})`:

1. `ws.img` missing ⇒ `current_workspace` returns `Some(Err("workspace image <path> is missing"))` (the task fails `workspace lost: …` on resume, as the host worker does when its directory is gone); `reconcile` returns `Unknown`.
2. Preflight (as above); collect dead inspect directories of the task (`jail::collect` on every `<home>/inspect/<task>/*`, then remove them); take `ws.lock`; create `<home>/inspect/<task>/<uuid>/` with `scratch.img` and, jailed, the staged jail (`id = inspect-<uuid>`, `vm.json` in the chroot) or, unjailed, `vm.json` in the directory; spawn the jailer (or Firecracker) in a **new process group of the controller**, `kill_on_drop`; connect (`<chroot>/v.sock` or `<dir>/v.sock`); `Hello{mode: inspect}`; one query; `Shutdown`; wait for exit ≤ 5 s else SIGKILL the group; `jail::collect`; remove the directory on success (kept, with `console.log`, `stderr.log` and `firecracker.log`, on failure for diagnosis; its jail is collected either way).
3. The whole call is bounded by `INSPECT_TIMEOUT` = 60 s; on timeout, launch failure or protocol failure: `current_workspace` ⇒ `Some(Err("workspace inspection failed: …"))`; `reconcile` ⇒ `Reconciliation::Unknown` (⇒ unreconcilable, exactly 3a's answer when the host cannot tell).
4. `Reconciliation::Applied` rebuilds the patch-result bytes with the same host code as a live `PatchApplied`, and `SupervisedExecutor::reconcile` retains it as a job receipt as today.

`current_workspace` stays synchronous on the `Executor` trait; the inspector blocks the controller for one boot (about 1–2 s measured for kernel boot plus agent start; the jailer adds milliseconds). Safety argument: an inspector only ever runs when every attempt of the task is settled (3a: `wait_for_job`, `fence_job`, `settled`), so `ws.img` has no other writer; the inspector itself writes nothing but the ext4 journal replay; its VM dies with the controller through the connection-EOF rule, and the jail it leaves in that case is collected by the next inspector of the task. Nothing about this moves into the guest's trust: the digest the inspector reports is computed by our agent from the image, the same way a job computes it.

### Attempt token and handles

The controller mints `attempt_token` (16 random bytes, 32 lowercase hex, `getrandom`) per job in `SupervisedExecutor::job_request` and writes it into `request.json`; the worker passes it in `Hello` over vsock, never on the kernel command line or on a drive. The guest binds the token to the attempt in `Hello` and refuses any other token for the life of the VM. Purpose: job identity (a stray connection to the wrong `v.sock`, an inspector reaching a job VM, a replayed `Hello`) — not authority. It is not a capability handle, it grants nothing, and it is readable by the same UID like everything else in the job directory (3a's stated position).

**No capability handle enters the guest.** Authority is decided on the host: the broker authorizes at intent and re-authorizes at dispatch (3a); the guest can only answer the one request the host sends it; its answer is accepted only through the receipt rules (`valid_outcome` in the supervisor, `check_outcome` and `accept_receipt` in the controller). Revocation mid-job is the 3a path unchanged: `revoke` drops `cancel`, the supervisor SIGKILLs the worker group (worker and Firecracker), the kill-receipt rule applies (a verification killed ⇒ `Failure("cancelled")`; a patch killed ⇒ no receipt ⇒ inspection reconciles it). The 3a note that "wrong-task, wrong-operation and unknown-handle denials become reachable when 3b hands a handle to the guest" is resolved as: those denials stay reachable only through the store's own paths; the guest is not a broker client in v0.1. If a later phase gives the guest something to present a handle to (Phase 4 puts the model broker on the host, so not then), the token slot in `Hello` is where it goes (owner decision 4: no handle in v0.1).

## Lifecycle and failure modes

### Job VM state machine (worker process)

```
Preflight ──fail──▶ Failure("firecracker worker unavailable: …")              [outcome written]
   │
   ▼
Prepare: ws.lock, ws.img (ReadSnapshot creates), scratch.img, vm.json (unjailed: <job>/vm.json)
   │ fail ─▶ Failure("cannot prepare the VM: …")                               [outcome written]
   ▼
Jail (Jailed only): plan, stage hard links + ownership, <chroot>/vm.json, cgroup marker
   │ fail ─▶ Failure("cannot prepare the jail: …")                             [outcome written]
   ▼
Spawn jailer (exec's firecracker) or firecracker ──spawn error──▶ Failure("cannot start firecracker: …")   [outcome written]
   │
   ▼
Booting: wait v.sock (<chroot>/v.sock or <job>/v.sock), CONNECT/OK, Hello/Ready   (≤ BOOT_TIMEOUT 15 s)
   │ timeout / process exited ─▶ kill VM; Failure("guest did not come up: …") [outcome written: no request was sent, so nothing changed]
   │     (a jailer failure before exec exits 1 with its reason on stderr ⇒ "guest did not come up: firecracker exit code 1"; stderr.log has the jailer's text)
   ▼
Serving: send the one request, stream files, await the reply
   │ Refused ─────────────────────▶ Failure(reason)                              [outcome written]
   │ reply ──────────────────────▶ Done
   │ EOF / protocol error / firecracker exited:
   │     Retry kinds (ReadSnapshot, RunVerification) ─▶ Failure("guest exited before reporting: firecracker exit code N")  [outcome written]
   │     ApplyPatch ─────────────▶ NO outcome; worker exits 1  ⇒ supervisor writes no receipt ⇒ reconciled (inspection)
   ▼
Done: Shutdown → wait exit ≤ 5 s else SIGKILL → remove scratch.img → release ws.lock → (after `run_vm` returns, in `run_blocking`) jail::collect (cgroup rmdir, chroot removal) → write outcome.json → exit 0
```

The jail is collected after the lock is released: this is safe because the controller serialises on `ws.lock` (one driver per home, no new job before this one's outcome), and the next boot's `collect_dead_inspections` runs under the lock.

At any point the supervisor may kill the worker group (lease, deadline, cancel): Firecracker dies with the worker; the 3a kill-receipt rule applies unchanged (a valid `outcome.json` present is published; else `Retry` kinds get a `Failure` receipt with the reason; `ReconcileThenRetry` kinds get none). A supervisor SIGKILL leaves the job dead by the lock; the controller's fence kills the session (worker and Firecracker) before `settled()` lets anyone reconcile or redispatch; the guest additionally shuts itself down on connection EOF. A job that died in any of these ways leaves its jail behind; the controller collects it once the job is settled ("The jail").

### Failure mapping

| Event | Retry kinds | ApplyPatch |
| --- | --- | --- |
| Preflight, prepare, jail staging, spawn, boot timeout (including a jailer that failed before exec: exit 1, reason in `stderr.log`) | `Failure` with the reason | `Failure` (nothing was sent to the guest) |
| Firecracker killed by the host OOM killer (cgroup `memory.max` with `memory.swap.max=0`) or by `SIGXFSZ` (`RLIMIT_FSIZE`) | `Failure("guest exited before reporting: firecracker killed by signal 9")` / `… exit code 151` | no outcome ⇒ reconcile |
| `Refused{reason}` | `Failure(reason)` | `Failure(reason)` (a validation failure before `git apply`, or `patch does not apply`) |
| Connection lost after the request was sent; Firecracker exit code 0 (guest panic/reboot) or 1, 2, 148–157 | `Failure("guest exited before reporting: firecracker exit code N")` | no outcome ⇒ reconcile |
| Frame over limit, malformed frame, wrong reply type | `Failure("guest protocol violation: …")` | unresolved if the request was sent, else `Failure` |
| `ws.lock` busy | `Failure("workspace image is attached to another VM")` | unresolved |
| Supervisor kill (lease/deadline/cancel) | 3a kill-receipt rule | 3a kill-receipt rule |
| Guest OOM kills the check | `Verified{exit_code: null}` ⇒ evidence with `passed: false` | n/a |
| Guest kernel panic (OOM of the agent, bug) | as "connection lost" (Firecracker exits 0 after the 1 s panic reboot) | as "connection lost" |

Lease arithmetic is unchanged (`EffectTimeouts` 70 s / 30 s); a boot costs 1–2 s of the lease and the check timeout stays 60 s, so the check's own `timeout` still normally wins over the lease, as 3a intends. `BOOT_TIMEOUT` (15 s) and `INSPECT_TIMEOUT` (60 s) are constants in `firecracker.rs`, overridable only in tests.

## Resource limits

| Resource | Mechanism | Bound |
| --- | --- | --- |
| CPU | `machine-config.vcpu_count = worker_vcpus`, `smt: false` | the guest cannot run more than `worker_vcpus` threads at once; a fork bomb or busy loops cost at most `worker_vcpus` host CPUs until the check timeout, lease or deadline kills the VM |
| Memory | `machine-config.mem_size_mib = worker_memory_mib`; no balloon | guest physical memory; the guest OOM killer kills the check first (`oom_score_adj` 1000 vs the agent's −1000); `GUEST_MIN_MEMORY_MIB = 128` so the image boots |
| Processes | `RLIMIT_NPROC=256` for `check`, process-group kill on timeout | a fork bomb saturates its vCPUs, not the host; the VM dies at the latest with the job |
| Disk | `ws.img` 1 GiB sparse, `scratch.img` 512 MiB sparse, `/tmp` and `/run` tmpfs 64 MiB (guest memory), root read-only | host disk use per task ≤ 1.5 GiB; `ENOSPC` inside the guest is an ordinary check failure |
| Wall clock | unchanged: check timeout 60 s, lease, deadline | — |
| Host-side Firecracker process (its own heap, vmm threads) | jailed: cgroup v2 `cpu.max = worker_vcpus × 100000 100000`, `memory.max = (worker_memory_mib + 128) MiB`, `memory.swap.max = 0`, `pids.max = 64`; `RLIMIT_FSIZE = 1 GiB`, `RLIMIT_NOFILE = 2048` (jailer default) | the process cannot use more CPU than the guest's vCPUs, more memory than the guest plus 128 MiB (the host OOM killer ends it: `guest exited before reporting: firecracker killed by signal 9`), more than 64 threads, or write a file over 1 GiB; measured overhead 45–60 MB at full guest occupancy; the KVM tier asserts `VmRSS ≤ mem_size_mib + 96 MiB` and `memory.events oom_kill == 0` in normal runs. Unjailed (`--allow-unjailed`): not capped. |

Concurrency bound: one driver per home (`driver.lock`) means at most **one job VM plus one inspector VM** per home at any time, so a home cannot fan out VMs.

## Security model

Trusted computing base: the host kernel and KVM, Firecracker v1.17.0 with its default seccomp filter, the jailer v1.17.0 (runs as root for milliseconds before dropping to the jail uid), the guest kernel (pinned CI artifact), the guest image bytes (pinned, registered read-only, re-digested before every launch), the `agentos-guest` agent and `git`/`python3` inside the guest, the controller, supervisor and worker. Untrusted: the repository snapshot, the patch, the verification check's behaviour (the profile bytes are trusted by pin, what the check does at run time is not), anything the guest kernel does after untrusted code runs, and — new with the jail — **the Firecracker process itself after a VM escape**: the jail is the second boundary, between a compromised VMM and the host.

| Threat | Defence in 3b-1 | Test (KVM tier unless noted) |
| --- | --- | --- |
| Guest attempts network access | No network device at all (`network-interfaces: []`); no MMDS; only `lo` in the guest | hostile profile `net-probe`: `python3 -c "socket.create_connection(('10.0.0.1',80),timeout=1)"` ⇒ `ENETUNREACH`; `/sys/class/net` lists only `lo`; evidence asserts both; the host's listening sockets are unchanged |
| Guest attempts host secret access | The guest sees only `vda` (image), `vdb` (its workspace), `vdc` (scratch) and `/dev/vsock` (root-only); no host path, no environment, no kernel-command-line secret; the controller spawns Firecracker with `env_clear()` | hostile profile `secret-probe`: the test plants `AGENTOS_TEST_SECRET=<random>` in the controller environment and the same string in `<home>/secret.txt`; the check greps `/proc/cmdline`, `/proc/*/environ`, every mounted fs and the raw block devices it can open; the evidence, `console.log` and the bundle contain no occurrence; opening `/dev/vsock` as `check` ⇒ `EACCES` |
| CPU exhaustion | `vcpu_count` | hostile profile `cpu-burn` (8 busy loops for 3 s in a 1-vCPU VM): the guest's `nproc` is 1 (evidence); the Firecracker process's `utime+stime` delta over the window ≤ 1.25 × window (from `/proc/<pid>/stat`); the check ends by its own exit |
| Memory exhaustion | `mem_size_mib`, guest OOM priorities | hostile profile `mem-hog` allocates `2 × worker_memory_mib`: the check is OOM-killed (`exit_code: null`, `passed: false`), the agent survives and reports, `VmRSS` of Firecracker ≤ `mem_size_mib + 96 MiB` throughout, the next effect's VM boots |
| Fork bomb | `RLIMIT_NPROC`, group kill, VM death | hostile profile `fork-bomb`: verification fails (`timeout` or non-zero exit); Firecracker exits within 5 s of `Shutdown` or is SIGKILLed; no process of the job survives (`/proc` scan, 3a helper) |
| Disk filling | bounded images, read-only root, tmpfs caps | hostile profile `disk-fill` writes until `ENOSPC` in `/tmp`, `/scratch/check` and tries `/workspace` (`EACCES`): sizes of `ws.img`/`scratch.img` (apparent and allocated) stay ≤ their caps; the root is unchanged (`mount -o remount,rw /` fails) |
| Patch escapes the workspace | unchanged 3a checks, now inside the guest, plus `/workspace` is the only writable tree `builder` has | conformance suite (fake guest): traversal, symlink, excluded component, non-editable path, binary patch |
| Tampered profile | pin checked on the staged copy inside the guest (`profile digest mismatch`) and by `check_inputs` on the host | conformance (fake guest) and KVM: a changed staged profile never yields `passed: true` |
| Stray or replayed control connection | `attempt_token` bound at `Hello`; one connection per VM | fake guest: a second connection with another token is closed; a `Hello` for another attempt is refused |
| Forged outcome bytes | all outcome bytes and receipts are built on the host; the guest's reply is data | unit: a `Verified` with `passed`-looking stdout but exit 1 is `passed: false`; receipt fields come from `request.json` only |
| VM escape into the Firecracker process (a Firecracker or KVM bug) | the jail: the process is uid 61000 (no file of the home but its own `ws.img`, `scratch.img`, `vm.json`, `firecracker.log` is writable; the journal, `request.json`, the registry and other tasks' images are root-owned or outside the chroot), chrooted via `pivot_root` in a private mount namespace (only the staged inodes are reachable, never a host path), inside a cgroup (`cpu.max`, `memory.max` + `memory.swap.max=0`, `pids.max`), with `RLIMIT_FSIZE` and Firecracker's own seccomp allowlist (no `fork`/`exec`, no `mount`, no `chroot`, no `open_by_handle_at`). Hard links expose inodes, not paths: the registry files are root `0444`, so the jailed process can read but not alter them, and the pre-launch re-digest would catch an alteration anyway | KVM tier: `jailed_firecracker_runs_as_the_jail_uid_in_its_chroot_and_cgroup` (`/proc/<pid>/status` uid/gid, cmdline `/firecracker … /vm.json`, `/proc/<pid>/cgroup` = `0::/agentos/<attempt>`); `jail_cgroup_limits_equal_the_contract` (`cpu.max`, `memory.max`, `memory.swap.max`, `pids.max` read from the cgroup); `memory_hog_firecracker_is_oom_killed_by_the_cgroup_when_the_bound_is_lowered` (test seam lowers `memory.max` below `mem_size_mib`: Firecracker dies by SIGKILL, `memory.events oom_kill == 1`, the effect fails `guest exited before reporting: firecracker killed by signal 9`, the next VM boots); `cpu_burn_is_throttled_by_the_cgroup_when_the_quota_is_lowered` (`cpu.stat nr_throttled > 0`, `usage_usec ≤ 0.6 × wall`); `fork_bomb_never_adds_a_host_process` (`pids.current ≤ 1 + vcpus + 4` sampled throughout); unit (no root): `jailer_argv_is_exactly_the_documented_one`, `stage_hard_links_and_marker_over_a_temp_home` |
| Jail leftovers after a crash | empty cgroup and chroot are inert; collected by the controller after settlement, by the inspector for dead inspect directories | fake jailer (no root): `supervisor_sigkill_leaves_the_jail_and_the_controller_collects_it_after_the_fence`; KVM tier: `leftover_jail_after_supervisor_sigkill_is_collected_on_resume` (cgroup gone, `jail/` gone, `scratch.img` blocks freed) |

Known limits of the security model (kept honest in the README): a VM escape (Firecracker or KVM bug) lands in the jail — as uid 61000 in a chroot and a cgroup, with Firecracker's seccomp filter — not on the host as the controller's UID; from there a second, kernel-level bug is needed to reach the host; unjailed (`--allow-unjailed`) the escape lands as the controller's UID; micro-architectural side channels are not mitigated beyond `smt: false` per VM (host SMT stays on); the jailed process has no PID or network namespace of its own (3b-2 if ever; it has no network device and cannot fork); one jail uid serves every VM of a home (at most one job VM and one inspector VM at a time, each in its own chroot); the host kernel (7.2) is newer than Firecracker's validated hosts; the guest agent is in the TCB; the job directory and `request.json` are readable and writable by the controller's UID (3a's position) and `ws.img` by the jail uid; the jailer runs as root (its own `main` is in the TCB for the duration of the setup); the guest's clock and entropy come from the host as any VM's do.

## Evidence and export compatibility

- Artifact bytes (`snapshot-manifest`, `patch-result`, `verification-evidence`, `effect-failure`) are produced by the same host functions as today from structured guest replies; `patch.diff`, `patches/`, `evidence/` and the manifest are unchanged for the host worker **byte for byte** (the CLI tests' `normalized()` comparison of crashed vs uncrashed bundles keeps passing for both workers).
- The manifest gains one optional field, `guest_image_digest: Option<Digest>` with `skip_serializing_if = "Option::is_none"`, so host-worker manifests do not change at all and Firecracker manifests name the image that produced the evidence. `status` prints `worker` and, for Firecracker, `guest_image`.
- Evidence from the Firecracker worker contains guest paths (`/workspace`, `/scratch/profile`), never host paths: the README limit "host paths can leak into exports" is narrowed to the host worker.
- `workspace_digest` values are identical across workers for the same tree (shared implementation; golden test: the fixture snapshot digest `be77aa19c032f85329a9596adfd692252a0c87fd09d337b1873feb6003bdd3b8` and profile digest `9ff584f31b7fef8ac5774ced5c8f1620c27f736b4bdc4d9e03e553df5d8ea12c` from the README must still be produced by `agentos_core::workspace::workspace_digest`, and the KVM tier asserts the guest's `SnapshotDone.workspace_digest` equals the host-computed digest of the staged snapshot).
- The crash matrix (`crates/agentos-engine/tests/crash_matrix.rs`) and the CLI crash demo table (`DEMO` in `crates/agentos-cli/tests/cli.rs`, 10 rows) run against the Firecracker worker in the KVM tier with the **same expected decisions, execution counts and lease generations** as 3a; `during-execute:apply_patch` still resumes by `PublishRetained`, and the exit-before-receipt rows by `PublishReconciled` (now through an inspection boot) and `Redispatch`.

## Testing

Tiers; every tier runs under `docker compose`.

**T0 — pure unit (`docker compose run --rm test cargo test --workspace`, no KVM):** frame encode/decode and every limit (boundary ±1); message serde round trips; `vm.json` rendering against golden files for both views (host paths and chroot paths; no network, `smt: false`, drive order); exit-code mapping; `FirecrackerConfig` validation (relative paths, `vcpus` 0/33, `memory_mib` 127); token format and per-job freshness; image registry (register, dedupe, `@`/traversal refused, read-only bits); contract `guest_image_digest` optional and byte-identical serialization without it; `agentos_core::workspace` golden digests; guest-side request validation (the `Refused` reason table) exercised in-process against directories; **the jail without root**: `JailPlan` layout and id validation (UUID ok; `inspect-<uuid>` ok; 65 characters, `_`, `/`, `.` refused), the exact jailer argv, `cgroup_values` for (1, 128), (4, 2048), (32, 65536), `find_cgroup2_root` over three `/proc/mounts` samples (host, container `ro`, none), `stage` over a temp "home" (every link has `nlink == 2`, ownership when running as root, the marker's content, `EXDEV` wording when the source is on another filesystem — `/dev/shm` vs the temp dir), `collect` over a temp dir (empty marker target removed; non-empty target reported; `ENOENT` tolerated; `jail/` gone), `decide` over the four combinations, and `probe` in the real environment of the tier (`test` service: `Err` containing `read-only`; non-root: `Err` containing `needs root`; `test-kvm`: `Ok`).

**T1 — fake guest (same command, no KVM):** `agentos-guest --fake <uds> <root_dir>` runs the real agent code as a host process: it listens on a Unix socket, implements Firecracker's `CONNECT <port>\n` / `OK <port>\n` handshake, and uses `<root_dir>/{workspace,scratch}` directories instead of block devices (no `mkfs`, no uid switch, no mounts — those are the KVM tier's job). `FirecrackerConfig.launcher = Fake` (honoured only with `AGENTOS_TEST_WORKERS=1`, passed with `Command::env`, never `set_var`) makes `FirecrackerWorker` and `Inspector` spawn it instead of Firecracker. On this seam:

- **Worker conformance suite** (`crates/agentos-engine/tests/worker_conformance.rs`): one table of cases run against `HostProcessWorker`, `FirecrackerWorker+Fake`, and (T2) `FirecrackerWorker+Real`: snapshot digest equals the host digest; patch applies and the result bytes equal the host worker's; version conflict; non-editable path; traversal; symlink; excluded component; binary patch; verification passes/fails with identical evidence fields; pinned-profile mismatch; `timeout`; oversized output truncation flags; `reconcile` answers `NotApplied`/`Applied`/`Unknown` on a base, a patched and a tampered workspace; `current_workspace` on a missing workspace. Outcomes are compared field by field after normalizing nothing: the bytes must be equal.
- Kill paths through the real supervisor binary: lease expiry kills the worker and the fake guest process; cancel marker; deadline; supervisor SIGKILL then fence settles the job and no process of it remains; EOF on the control connection shuts the fake guest down within 500 ms.
- Failure mapping table: boot timeout (fake guest told to never listen), EOF after `ApplyPatch` was sent ⇒ no outcome ⇒ the supervisor writes no receipt ⇒ `SupervisedExecutor::run` reconciles through the fake inspector; EOF after `RunVerification` ⇒ `Failure("guest exited before reporting…")`; a second `Hello` with another token is refused; a frame over the limit is a protocol failure; `ws.lock` held ⇒ the stated outcomes.
- The crash matrix and the CLI crash demo table run with `AGENTOS_TEST_WORKER=firecracker-fake` (the engine's `common::supervised` and the CLI's `Cli::cmd` honour it) with unchanged expectations.
- **The fake jailer** (`crates/agentos-engine/tests/jail.rs`, and the conformance and kill-path suites in `firecracker-fake-jailed` mode): the test writes a `#!/bin/sh` script into a temp dir and names it as `jailer_bin` in `JailMode::Jailed(JailConfig { uid: geteuid(), gid: getegid(), cgroup_root: <temp dir> })`. The script parses the documented argv, appends it to `<dir>/jail/argv.txt`, `mkdir -p`s the "cgroup" `<cgroup_root>/agentos/<id>`, `cd`s into the chroot and `exec`s `agentos-supervisor fake-guest <chroot>/v.sock <work_root>/<task>` (exec in place, like the jailer). So the worker's jailed path — plan, stage (real hard links and ownership, to the test's own uid), marker, argv, connect to the chroot socket, `collect` — and the controller's collection after a fence are exercised on every `cargo test --workspace` without root or KVM. What the fake jailer cannot show (the real chroot, uid drop, cgroup, KVM) is the KVM tier's job.

**T2 — KVM-gated (`docker compose run --rm test-kvm cargo test --workspace`; the tests gate on `AGENTOS_KVM_TESTS`, not on `#[ignore]`):** the `test-kvm` service is the `test` service plus the settings in "The jail" (`/dev/kvm`, `CAP_SYS_ADMIN`, the seccomp profile, the cgroup-delegation entrypoint), `AGENTOS_KVM_TESTS=1`, `AGENTOS_FIRECRACKER`, `AGENTOS_JAILER`, the `guest-images` volume and `scripts/fetch-firecracker.sh` output. Every VM of the tier is **jailed** (the probe succeeds there); the unjailed path is covered by one test that passes `--allow-unjailed` and asserts `jailed: false` in `Submitted` and a Firecracker process running as the container's uid without a cgroup of its own. KVM tests begin with `let Some(kvm) = kvm::require() else { return };` where `require()` returns `None` and prints `SKIPPED: set AGENTOS_KVM_TESTS=1 and pass /dev/kvm (docker compose run --rm test-kvm …)` when the variable is unset, and **panics** when it is set but `/dev/kvm`, the binary or the image is missing, so a KVM-expected environment can never pass silently. T2 contents: the conformance suite on the real guest; the threat-model table above, the jail rows included (uid, chroot, cgroup, limits equal to the contract, OOM kill and CPU throttling under lowered bounds through the test seams `with_jail_memory_max_mib`/`with_jail_cpu_quota_us`, no host process added by a fork bomb, leftover collection after a supervisor SIGKILL, the `--allow-unjailed` run, `jailer unavailable` refusal with `AGENTOS_JAILER=/nonexistent` exiting 1 before the task is touched); image build reproducibility (`build-guest-image.sh --verify`); boot time budget (`Ready` within 5 s on this host, asserted ≤ `BOOT_TIMEOUT`); the crash matrix and CLI demo table on the real worker; `scripts/demo.sh --worker firecracker` producing the README transcript; inspection after a killed patch VM (test hook `AGENTOS_TEST_KILL_VM_AFTER_REQUEST=1`, honoured only with `AGENTOS_TEST_WORKERS=1`: the worker SIGKILLs Firecracker right after sending `ApplyPatch`, so the image holds either the base or the base plus the patch; the worker writes no outcome, the supervisor no receipt, and recovery reconciles by inspection with the decision `PublishReconciled` or `Redispatch` according to what the image holds; the task converges to SUCCEEDED and the journal's digest equals the inspector's); `current_workspace` on resume boots an inspector and the task continues; concurrency bound (never more than two Firecracker processes under the home during the whole demo).

What is verified only with KVM: real boot, `mkfs`/mount, uid separation, OOM behaviour, CPU and memory bounds, the no-network guarantee, host-secret isolation, squashfs read-only root, journal replay in the inspector, and the real jail (chroot, uid drop, cgroup limits and their enforcement). Everything else (protocol, worker, inspector plumbing, kill paths, outcome bytes, recovery decisions, jail planning, staging, argv, collection) is verified in T0/T1 on every `cargo test --workspace`.

Flakiness control: as in 3a, the supervisor and crash-matrix suites run 20× in one container before a task is done; the KVM tier 10×; leases in kill tests are hundreds of milliseconds with real clocks; boot timeouts are generous (15 s) relative to the measured 1–2 s; the kernel download and the Firecracker tarball are verified by sha256 and cached in compose volumes; the image build is deterministic and cached by digest; no test depends on `v.sock` appearing within a fixed time other than `BOOT_TIMEOUT`.

## Acceptance for 3b-1

The build plan's Phase 3 rows left open by 3a: "guest attempts network access or host secret access — the reference worker configuration provides neither resource" (threat-model rows 1–2 above, KVM tier) and "repository code stays within its workspace and configured resources" (rows 3–6; workspace containment by uid and mount layout). Plus: every 3a acceptance row still passes against the Firecracker worker (crash matrix, CLI demo table, revoke mid-job stops the VM, deadline kills the VM, duplicate/late receipts); the conformance suite shows the two workers are observationally equal on the fixture; the Firecracker process of every VM in the KVM tier runs jailed (uid 61000, chroot, cgroup with the contract's limits) and the jail's leftovers are collected after a supervisor SIGKILL; `cargo test --workspace` stays green without KVM or root beyond the container's own; the README transcript for the Firecracker demo is real output.

## Rollout and README

- Default worker stays `host`; `--worker firecracker` opts in per task; the recorded worker and the recorded `jailed` drive later commands. `scripts/demo.sh` gains `--worker firecracker` and `scripts/fetch-firecracker.sh` plus `scripts/build-guest-image.sh` are documented in "Build and test" with the `test-kvm` service and the reason for each of its settings (`CAP_SYS_ADMIN`, the seccomp profile, the cgroup entrypoint). Running `--worker firecracker` on the host as a non-root user is documented as: either `docker compose run --rm test-kvm …`, or `--allow-unjailed` with its consequence spelled out.
- README "Known limits": **"Not sandboxed"** becomes "Sandboxed only with `--worker firecracker`" (host worker: unchanged text); **"`reconcile` runs in the controller"** becomes "inspection boots a VM from the controller process (bounded, read-only on the workspace); a missing `/dev/kvm` or jailer at resume fails the command before the task is touched"; **"Host paths can leak into exports"** narrowed to the host worker; add: "jailed Firecracker needs root and a writable, delegated cgroup v2 tree; `--allow-unjailed` runs it as the controller's UID without chroot or cgroup and records `jailed: false`", "the jailed process has no PID or network namespace (3b-2)", "one jail uid (61000) per home", "workspace and scratch images are not garbage-collected (the jails are)", "the host kernel is newer than Firecracker's validated hosts", "one VM boot per effect (no VM reuse)". The "Phase 3b (not built yet)" section is replaced by a "Phase 3b-2 (not built yet)" list.

## Known limits (after 3b-1)

- The jail needs root and a writable, delegated cgroup v2 hierarchy; without them `--worker firecracker` refuses unless `--allow-unjailed`, and unjailed the controller's UID is the blast radius of a VM escape. Jailed, the blast radius is uid 61000 in a chroot and a cgroup; the process has no PID or network namespace of its own (3b-2 if ever).
- The jail hard-links `ws.img`, `scratch.img` and the registered image into the chroot, so the home must be one filesystem mounted without `nodev`/`noexec` (`/tmp` on most hosts is `nodev`: not a valid home for jailed use).
- One jail uid serves every VM of the home; `ws.img` of a jailed task belongs to it, so a home used jailed must stay used as root.
- `ws.img` and `scratch.img` sizes are constants (1 GiB / 512 MiB), not contract limits; images are never collected (jails are).
- One boot per effect (1–2 s each) and one inspector boot per resume and per receipt-less patch; no VM reuse or snapshots.
- The guest image build needs network access (snapshot.debian.org, the Firecracker CI bucket) and root in the build container; the kernel is a pinned download, not built from source.
- The preflight refuses a command (exit 1, task untouched) when KVM, the binary, the jailer, the cgroup tree or the image is unusable; an inspector that fails after the preflight passed (the device vanished, a boot failure, a jailer failure) fails the task on resume with `workspace lost: workspace inspection failed: …`. Rare, but a task can fail for an infrastructure reason.
- The KVM test tier's container needs `CAP_SYS_ADMIN`, a seccomp profile that allows `pivot_root` and a cgroup-delegation entrypoint; the default `test` service needs none of it, and no test of the default tier depends on root beyond what the container already has (the fake jailer stages to the test's own uid).
- No aarch64; x86_64 only (the image recipe pins the x86_64 kernel).
- The guest trusts the host's time (`kvm-clock`) and the serial console is the only guest log.

## Owner decisions (2026-10-02)

The eight questions this document was proposed with, each with the owner's answer, recorded on 2026-10-02. None is open.

1. **Guest base distribution** — Debian bookworm from `snapshot.debian.org` at a pinned timestamp (exact reproducibility, glibc Python). Alpine rejected.
2. **Disk image sizes** — constants in 3b-1 (`WS_IMAGE_BYTES` = 1 GiB, `SCRATCH_IMAGE_BYTES` = 512 MiB); `worker_disk_mib` stays a 3b-2 contract field (optional, `skip_serializing_if`, so digests of existing contracts stay).
3. **Jailer** — **in 3b-1**, through the official `jailer`: chroot, uid/gid drop, cgroup v2 limits, Firecracker's default seccomp; without `--new-pid-ns`/`--daemonize`/`--netns` (reasons in "Decisions at a glance" and "The jail"). Unavailable ⇒ refuse by default, `--allow-unjailed` runs unjailed and records `jailed: false`. The compose `test-kvm` settings and the behaviour of every step were verified by experiment ("Jailer experiments"). The per-VM CPU and memory limits are enforced twice (VM config and cgroup) and tested both ways.
4. **Capability handle in the guest** — none in v0.1; `Hello` keeps the token slot (`attempt_token`), which carries identity, not authority.
5. **Inspector** — runs in the controller process (read-only use, bounded 60 s, dies with the controller through connection EOF, no job-directory churn); jailed like a job VM.
6. **Guest kernel** — the pinned CI artifact `vmlinux-6.18.51` (sha256 above; supported to 2028; booted here). Building from source is 3b-2 / Phase 6.
7. **Host kernel 7.2 outside Firecracker's validated list** — accepted; `Submitted` records `host_kernel` and `firecracker_version` so a later failure is attributable.
8. **`ws.img` cache mode** — `Writeback` (guest `fsync` reaches the host file; needed for the reconcile argument).

Also approved on 2026-10-02: the new dependency `base64` 0.23.1; outbound network for the image build and the pinned downloads (snapshot.debian.org, the Firecracker CI bucket, the GitHub release); the three plan resolutions — a post-send `ApplyPatch` failure of any kind reconciles by inspection (no `unresolved` outcome except the `ws.lock`-busy case before anything was sent), `--worker` is optional and defaults to the recorded worker for every command but `submit`, and the fake guest is reachable as `agentos-supervisor fake-guest`/`agentos supervise fake-guest` with `agentos-engine` depending on the `agentos-guest` library; execution of the plan is subagent-driven.

Defaults chosen by this document that the owner may still override without a design change: the jail uid/gid value `61000` (a flag and an env exist), the deny-list shape of `scripts/kvm-seccomp.json` (a full copy of Docker's default profile with `pivot_root` added would be narrower by a few dozen rarely used syscalls at the cost of a 900-line file), `JAIL_MEMORY_OVERHEAD_MIB = 128`, `pids.max = 64`.

## Self-review

- Placeholder scan: no TBD/TODO; every "later" item is assigned to 3b-2; every former open question has its owner's answer.
- Contradictions checked: Firecracker is in the worker's group (not `process_group(0)`), so the 3a `worker_pgid` kill covers it, and `groups` needs no entry — and the jailer preserves pid, pgid, sid and stdio because it exec's in place without `--new-pid-ns`/`--daemonize` (verified in E10/E11), so this still holds jailed; the inspector attaches `ws.img` read-write only for journal replay and remounts read-only, consistent with "the inspector writes nothing but the journal replay"; `ApplyPatch` never produces a `Failure` after its request was sent (no outcome ⇒ inspection; the `ws.lock`-busy unresolved outcome happens before any send), consistent with the 3a kill-receipt rule; manifests change only by one optional field; the contract schema gains only an optional, non-serialized-when-absent pin; `Submitted` gains `worker`, `guest_image_id`, `guest_image_digest`, `firecracker_version`, `host_kernel`, `jailed` and nothing else; the memory bound in the resource table, the threat table and "The jail" is the same number (`memory_mib + 128 MiB`, swap 0); the file names in the home layout, the jail layout, the spawn description and the lifecycle agree (`console.log` = stdout, `stderr.log` = stderr, `firecracker.log` = the logger file, hard-linked when jailed).
- Scope: 3b-1 is the worker, image, protocol, inspection, selection, the jail and the tests; 3b-2 is rate limiting, sizes, GC, the kernel build and the jailer extras that need another supervision model. The first plan covers 3b-1.
- Ambiguities resolved inline: which process spawns Firecracker and through what argv, where every file lives in both modes, who owns it, every limit and timeout as a number, every reason string, every exit-code mapping, who writes which byte of an outcome, who collects what after which kind of death, and the exact container settings with the experiment that justifies each.
