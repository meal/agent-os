# Phase 3b: Firecracker worker and guest workspace — design

Date: 2026-10-02
Status: proposed; environment facts below were measured on this host on 2026-10-02.
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
| **3b-1** (first plan) | `agentos-guest` crate and guest image build; image registry and pinning; vsock control protocol with a fake guest for host-only tests; `FirecrackerWorker`; per-task workspace image; inspection boots for `reconcile`/`current_workspace`; vCPU and memory from the contract; no NIC; worker selection and recording; KVM-gated test tier (`test-kvm` compose service); README. |
| **3b-2** (second plan) | jailer mode when running as root (chroot, uid/gid drop, new pid namespace, cgroup v2 `cpu.max` and `memory.max` for the Firecracker process itself); block-device rate limiting; contract-driven disk sizes (`worker_disk_mib`); garbage collection of workspace and scratch images with the job directories; building the guest kernel from source (the build plan's Phase 6 "reproducible guest image build" finishes there). |

## Environment facts (measured 2026-10-02)

Host (`stark`, Arch Linux, kernel `7.2.7-arch1-1`, x86_64, 32 CPUs, 64 GiB):

- `/dev/kvm` exists: `crw-rw-rw- 1 root kvm 10, 232`; `open(O_RDWR)` succeeds for the dev user (uid 1000, not in `kvm`; the mode is world-rw). `/proc/cpuinfo` has `vmx`; `kvm_intel` and `kvm` are loaded; `systemd-detect-virt` says `none` (bare metal, **no nested virtualization needed**). `/dev/vhost-vsock` exists but is not needed (Firecracker implements vsock in user space).
- `firecracker` and `jailer` are not installed on the host; nothing was installed system-wide for this spec.
- Docker 29.8.2, Compose 5.5.1.

Compose `test` container (`rust:1.98.1-bookworm`, runs as root, `init: true`):

- **By default `/dev/kvm` is not visible** (`ls: cannot access '/dev/kvm'`).
- With an override file adding `devices: ["/dev/kvm:/dev/kvm"]` to the `test` service, `/dev/kvm` is visible and `open(O_RDWR)` succeeds. The container's cgroup v2 tree is mounted **read-only** (`/sys/fs/cgroup ... ro`), so the jailer's cgroup creation cannot work there (one reason the jailer is 3b-2).
- The container has `mkfs.ext4` (e2fsprogs), `git`, `python3`, `curl`; it lacks `mksquashfs`, `mmdebstrap`, `debootstrap` and the `x86_64-unknown-linux-musl` Rust target (only `x86_64-unknown-linux-gnu` is installed). The `Dockerfile` must add them (below).

Firecracker (GitHub releases API, 2026-10-02):

- Latest release **v1.17.0**, published 2026-09-10 (same day as v1.16.2; earlier: v1.16.1 2026-07-02, v1.16.0 2026-06-04, v1.15.1 2026-04-07). The 3a plan's "v1.17.0 as of 2026-10-02" is confirmed.
- `firecracker-v1.17.0-x86_64.tgz` sha256 `06094a1108ae9e82aa4c23a775aa92758f53f1175d422270d9d6162cb9ade558`; it contains the static musl `firecracker` (sha256 `99ad0f5cd0514a88aad0e9ae8cfdb3cc3b4ab9d190e1194602406c786b5de7a5`), `jailer` (`65ef226e96f0ceda55ba643f445801ef2cc0ea667ef67cad8ac4f406c9c8434f`), `seccompiler-bin`, `cpu-template-helper`, `snapshot-editor`, `rebase-snap`, the default seccomp filter JSON and CPU templates.
- Kernel support policy: validated host kernels 5.10, 6.1, 6.18; validated guest kernels 5.10, 6.1, 6.18 (6.18 supported until at least 2028-06-01). **Our host kernel 7.2 is newer than any validated host kernel** (risk, below); it worked in the smoke test.
- Recommended guest artifacts: the Firecracker CI bucket `https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/<date>-<sha>/x86_64/`; the newest prefix on 2026-10-02 is `firecracker-ci/20260930-a738f18a8db0-0/`, offering `vmlinux-5.10.268`, `vmlinux-6.1.186`, `vmlinux-6.18.51` (each with its `.config`) and `ubuntu-24.04.squashfs`. `vmlinux-6.18.51` sha256 `0545ba1781fc06cfa1d7699069057f4538103fd1644100cf0da434899a1ed447`; its config has `EXT4_FS=y`, `SQUASHFS=y` (zstd, xz), `OVERLAY_FS=y`, `TMPFS=y`, `DEVTMPFS=y`, `VIRTIO_BLK=y`, `VIRTIO_VSOCKETS=y`, `VIRTIO_MMIO=y`, `CGROUPS=y`, `MEMCG=y`, `PID_NS=y`, `USER_NS=y`, `SECCOMP=y`, `SERIAL_8250_CONSOLE=y`. Firecracker's docs recommend an uncompressed `vmlinux` on x86_64, boot args `console=ttyS0 reboot=k panic=1`, and note that a guest `reboot` makes Firecracker exit (no guest power management).
- `firecracker --no-api --config-file FILE` boots from one JSON (fields named as in the API: `boot-source`, `drives`, `machine-config`, `vsock`, `network-interfaces`, `logger`, …) and the process **exits when the guest shuts down** (the no-API loop breaks on `FcExitCode::Ok`). Exit codes: 0 ok, 1 generic error, 2 unexpected error, 148 bad syscall (seccomp), 149 SIGBUS, 150 SIGSEGV, 151 SIGXFSZ, 152 bad configuration, 153 argument parsing, 154 SIGXCPU, 155 SIGPIPE, 156 SIGHUP, 157 SIGILL. The default seccomp filter is applied with or without the jailer; `--no-seccomp`/`--seccomp-filter` are not recommended for production and are not used.
- API limits (swagger): `vcpu_count` 1..=32, `mem_size_mib` integer, `smt` default false, `huge_pages` default None; `Drive{drive_id, is_root_device, is_read_only, path_on_host, cache_type: Unsafe|Writeback (default Unsafe), io_engine: Sync|Async, rate_limiter}`; `Vsock{guest_cid >= 3, uds_path}`; `Balloon{amount_mib, deflate_on_oom}` (not used).
- vsock: the guest's `AF_VSOCK` ports map 1:1 to host `AF_UNIX` sockets. Host-initiated: connect to `uds_path`, send `CONNECT <port>\n`, read `OK <hostport>\n`; if nobody listens in the guest Firecracker closes the connection. Guest-initiated connections go to `uds_path_<port>` on the host (not used: the guest never initiates).
- Jailer: must run as root; copies the binary into `<chroot_base>/firecracker/<id>/root/`, `pivot_root`s there, mknods `/dev/kvm`, writes cgroups (`--cgroup key=value`, `--cgroup-version 2`), `--resource-limit fsize=|no-file=`, `--new-pid-ns`, `--daemonize`, drops to `--uid/--gid`, writes `firecracker.pid`, forwards arguments after `--` (so `--config-file` works relative to the jail).

**Smoke test (real, in the compose container with `/dev/kvm` passed through):** `firecracker --no-api --config-file vm.json` with v1.17.0, `vmlinux-6.18.51`, 1 vCPU, 128 MiB, a read-only 8 MiB ext4 root drive, a vsock device and no network interface: the kernel booted (`Linux version 6.18.51+`, `virtio_blk virtio0: [vda]`, `NET: Registered PF_VSOCK`), mounted the root read-only, ran `/sbin/init`, panicked "No working init found", rebooted after 1 s and **Firecracker exited 0 after 1649 ms** ("Firecracker exiting successfully. exit_code=0"); the host socket `v.sock` had been created in the config directory. Firecracker appended `root=/dev/vda ro` and the `virtio_mmio.device=` entries to the command line itself. So Firecracker can run in the dev environment **when** the compose service gets `/dev/kvm`; nothing else (no nested virt, no root on the host, no jailer) is required.

Crate versions (`cargo search`, 2026-10-02; `cargo search` sends no identity): `vsock` 0.5.4 (guest side, `AF_VSOCK` sockets), `rustix` 1.1.5 (already used; `mount`, `process`, `fs` features for the guest init), `tokio-vsock` 0.7.2 (not used: the guest agent is synchronous), `firec` 0.2.0 (not used: with `--no-api` no API client is needed), `hyper` 1.11.1 / `hyperlocal` 0.9.1 (not used, same reason). Re-check before pinning.

## Non-goals

Model broker (Phase 4); the Wasm component ABI (Phase 5); multi-host or remote workers; VM snapshots, restore and live migration; a long-lived VM per task or a VM pool; GPU; a guest network of any kind (not even a host-only tap); guest-initiated vsock connections; aarch64; the jailer, cgroups for the Firecracker process, uid/gid drop and PID-namespace isolation (3b-2); image garbage collection (3b-2; `<home>/jobs` GC remains the 3a known limit); `worker_disk_mib` in the contract (3b-2); building the guest kernel from source (3b-2); any change to the journal schema, reducer, broker rules, lease arithmetic, or the job-directory liveness rule.

## Decisions at a glance

| Question | Decision | Rejected |
| --- | --- | --- |
| Where Firecracker runs | As the **worker process's child**, in the worker's own process group, under the unchanged 3a supervisor. | Supervisor spawning Firecracker directly (changes the supervisor and the kill paths for one worker); a VM daemon (3a: no daemon). |
| VM granularity | **One microVM per job** (effect attempt), booted fresh, destroyed when the job ends. | One VM per task (needs an owner outliving jobs and controllers; snapshot/restore to survive restarts). |
| Firecracker API | `--no-api --config-file <job>/vm.json`; no HTTP client, no API socket. | REST over the API socket (a dependency and a second control channel for nothing we need). |
| Jailer | **Not in 3b-1** (needs root and a writable cgroup fs; neither exists in the dev setup). Firecracker's default seccomp filter stays on. 3b-2 adds `--jailer` mode. | Requiring root for 3b. |
| Guest kernel | Firecracker CI `vmlinux-6.18.51`, downloaded by pinned URL and sha256 at image build. | Building a kernel (3b-2); 6.1 (shorter support). |
| Guest rootfs | Debian bookworm `mmdebstrap --variant=apt` from `snapshot.debian.org` at a pinned timestamp, plus `python3 git e2fsprogs`, packed as a **read-only squashfs** (zstd, deterministic flags). | Alpine (no snapshot archive, musl Python); ext4 rootfs (writable by construction; would need `ro` discipline). |
| Guest init | `agentos-guest`, our static musl binary, is `/sbin/init` (PID 1) and the only service. | systemd/OpenRC (boot time, surface). |
| Control channel | **vsock**, one host-initiated connection per VM, length-prefixed frames, JSON control + raw byte frames. | Serial console (no framing, shared with kernel output); a block device mailbox (polling, no backpressure); 9p/virtio-fs (not in Firecracker). |
| Workspace | A **per-task ext4 block image** `<home>/work/<task>/ws.img`, created sparse by the host, formatted and mounted only by the guest. | Host directory shared into the guest (no shared-fs device exists); re-sending the workspace every boot (no durable guest state to reconcile). |
| `reconcile` / `current_workspace` | **Inspection boot**: the same image in `inspect` mode, read-only use of `ws.img`, run by the controller, bounded, self-terminating. | Host loop-mount (needs root/`CAP_SYS_ADMIN`); parsing ext4 in the controller (a second implementation of the digest's view of the tree). |
| Handles in the guest | **No capability handle enters the guest.** The guest gets a random per-job `attempt_token` for job identity only. | Passing the real handle (a bearer secret readable by untrusted code, with nothing in the guest to present it to). |
| Resource limits | `vcpu_count = worker_vcpus`, `mem_size_mib = worker_memory_mib`, `smt=false`; guest-side `RLIMIT_NPROC` and OOM priorities; bounded disks. | Balloon (`deflate_on_oom` is a courtesy, not a bound); cgroups (3b-2). |
| Worker selection | Global CLI flag `--worker host|firecracker` (env `AGENTOS_WORKER`), recorded in `Submitted`; later commands use the recorded worker. | A contract field (the contract describes the task, not the host's backend). |
| Tests without KVM | A **fake guest** (the real `agentos-guest` code running as a host process on a Unix socket that mimics Firecracker's `CONNECT` handshake) drives the whole host side; KVM-gated tests run in a `test-kvm` compose service and **skip loudly** elsewhere. | Mocking the worker (would not test the protocol or the kill paths). |

## Architecture

New and changed units. Controller, journal, reducer, recovery, broker and supervisor keep their shape; the `Executor` trait is unchanged.

| Unit | Kind | Responsibility |
| --- | --- | --- |
| `agentos-core/src/workspace.rs` | moved from `agentos-engine/src/workspace.rs` (engine re-exports it, so no import changes) | The one implementation of `workspace_digest`, `copy_tree`, `list_files`, exclusions. Host and guest link the same code, so digests are identical by construction. |
| `agentos-core/src/guest.rs` | new | The control protocol: message types, framing constants, limits, `GUEST_PROTOCOL = 1`, `VSOCK_PORT = 5200`, `GUEST_CID = 3`. Pure types, serde. |
| `crates/agentos-guest` | new binary crate, built for `x86_64-unknown-linux-musl` (static) and for the host target (fake mode) | The guest init and agent: mounts, drives, uid separation, request execution, inspection queries, watchdog, shutdown. Dependencies: `agentos-core`, `serde`, `serde_json`, `rustix` (`mount`, `process`, `fs`, `net`), `vsock`. No tokio. |
| `agentos-engine/src/firecracker.rs` | new | `FirecrackerConfig`, `FirecrackerWorker` (implements `Worker`), `vm.json` rendering, process spawn, exit-code mapping, preflight, `Inspector`. |
| `agentos-engine/src/guestlink.rs` | new | Host side of the protocol: Unix-socket connect with the `CONNECT`/`OK` handshake, framing, timeouts, and the `GuestLauncher` seam (`Real` Firecracker or the fake guest). |
| `agentos-engine/src/job.rs` | changed | `WorkerConfig::Firecracker(FirecrackerConfig)`; path validation as for `HostConfig`. |
| `agentos-engine/src/supervised.rs` | changed | `reconciler` becomes `Reconciler::{Host(FixtureExecutor), Firecracker(Inspector)}`; `job_request` mints a fresh `attempt_token` per job for Firecracker configs. |
| `agentos-cli` | changed | Global `--worker`, `--firecracker PATH`; `agentos image register DIR \| list`; `Submitted` records the worker and image; `Home::executor` builds the matching executor and runs the preflight. |
| `guest/python-stdlib-v1/` | new, in the repo | The image recipe: `kernel.lock` (URL + sha256), `packages.txt`, `build.sh`, `image.json` template. |
| `scripts/build-guest-image.sh`, `scripts/fetch-firecracker.sh` | new | Reproducible image build under compose; pinned Firecracker download into `build/firecracker/v1.17.0/` (git-ignored, never system-wide). |
| `compose.yaml`, `Dockerfile` | changed | `test-kvm` service (`devices: ["/dev/kvm:/dev/kvm"]`, `AGENTOS_KVM_TESTS=1`); image gains `mmdebstrap squashfs-tools` and the musl target. |

### Process model

```
controller (agentos CLI)                        [holds driver.lock]
  └─ agentos supervise run <job>  (session leader, subreaper, holds <job>/lock via stdin)
       └─ agentos supervise worker <job>  (own process group = worker_pgid)
            └─ firecracker --no-api --config-file <job>/vm.json --id <attempt>   (same group)
                 └─ guest: agentos-guest (PID 1) ─┬─ git apply       as uid 1000 (builder)
                                                 └─ check command   as uid 1001 (check)
```

Firecracker is a plain child of the worker in the worker's process group, so every 3a kill path reaches it unchanged: the supervisor's `kill_process_group(worker_pgid)` on lease, deadline or cancel; `reap_all` as subreaper; the controller's `kill_job`/`kill_session` fence after a supervisor SIGKILL; `settled()` refusing to settle while anything of the job's session lives. A SIGKILLed Firecracker process destroys its VM instantly (the KVM VM is a kernel object owned by the process). Nothing in the guest can outlive the Firecracker process.

Belt and braces: the guest agent **shuts the VM down when its control connection closes** (EOF on the vsock connection → `sync`, unmount, `reboot(RB_AUTOBOOT)`; with `reboot=k` Firecracker exits 0). So a worker that dies for any reason takes its VM down within milliseconds even before the supervisor's kill lands, and an inspector VM dies with the controller that opened it.

### Home layout additions

```text
<home>/bin/firecracker                   optional: the pinned Firecracker binary (see --firecracker)
<home>/registry/images/<id>@<digest>/    registered guest images, read-only: image.json, vmlinux, rootfs.squashfs
<home>/registry/images/<id>@<digest>.meta.json
<home>/work/<task>/ws.img                the task workspace block image (sparse ext4, WS_IMAGE_BYTES)
<home>/work/<task>/ws.lock               advisory lock: who has ws.img attached
<home>/jobs/<effect>-<attempt>/vm.json   Firecracker configuration (worker)
<home>/jobs/<effect>-<attempt>/v.sock    vsock host socket (created by Firecracker)
<home>/jobs/<effect>-<attempt>/scratch.img   per-job scratch drive (sparse, SCRATCH_IMAGE_BYTES)
<home>/jobs/<effect>-<attempt>/console.log   guest serial console (kernel + agent log)
<home>/jobs/<effect>-<attempt>/firecracker.log
<home>/inspect/<task>/<uuid>/            inspector VMs: vm.json, v.sock, scratch.img, console.log (removed on success)
```

The job-directory single-writer rule extends: `vm.json`, `scratch.img`, `console.log`, `firecracker.log` belong to the worker (Firecracker writes the last three on the worker's behalf), `v.sock` to Firecracker. `JobDir::list` keeps ignoring nothing new: these names never collide with the protocol files.

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
}
```

`JobDir::create` and `run_worker` reject relative paths exactly as for `HostConfig`. `vcpus` and `memory_mib` are validated at `submit` for the Firecracker worker (`worker_vcpus <= 32`, `worker_memory_mib >= 128`, exit 2 with the limit named); the contract schema itself does not change.

Selection: `agentos --worker firecracker submit …` (or `AGENTOS_WORKER=firecracker`). `submit` resolves the guest image from `contract.profile` (the build plan's repository profile, e.g. `python-stdlib-v1`) exactly as `resolve_profile` resolves verification profiles: an optional contract pin `guest_image_digest` (`#[serde(default, skip_serializing_if = "Option::is_none")]`, 64 lowercase hex, so existing contract digests do not change) ⇒ exactly that registry entry else exit 2; else the newest registry entry for the id; no legacy directory fallback. The `Submitted` payload gains `worker` (`"host"` or `"firecracker"`) and, for Firecracker, `guest_image_id`, `guest_image_digest`, `firecracker_version` and `host_kernel` (from `uname -r`, for attribution; see open question 7). `resume`, `cancel`, `revoke`, `status` and `export` build the executor from the recorded worker; a `--worker` flag that disagrees with the record exits 2 (`task was submitted with worker host`). The `profile` field is ignored by the host worker as today.

`--firecracker PATH` (env `AGENTOS_FIRECRACKER`) names the binary; default `<home>/bin/firecracker`. `scripts/fetch-firecracker.sh [DEST]` downloads the v1.17.0 tarball, verifies the tarball and binary sha256s above and installs `firecracker` (and `jailer`, for 3b-2) into `DEST` (default `build/firecracker/v1.17.0/`). Nothing is installed outside the repo or the home.

### Preflight

`Home::executor` for a Firecracker task runs `FirecrackerWorker::preflight(&config)` before anything is journaled: `/dev/kvm` opens read-write; `firecracker_bin` is executable and `--version` prints `Firecracker v1.17.`; `image_dir/image.json` parses, has `protocol == GUEST_PROTOCOL` and names existing `vmlinux` and `rootfs.squashfs`; `workspace_digest(image_dir) == image_digest`. A failure exits the command with code 1 and the reason (`firecracker worker unavailable: /dev/kvm: Permission denied`) **before** the task is touched, so a missing KVM never fails a task. The worker repeats the same checks before every launch (defense in depth; a failure there is `Failure("firecracker worker unavailable: …")`, journaled like any effect failure).

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

## Protocols

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

No `balloon`, no `mmds-config`, no `entropy` beyond the kernel's own, no `cpu-config`. The worker spawns `<firecracker_bin> --no-api --config-file <job>/vm.json --id <attempt_id>` with `env_clear()`, cwd `<job>`, stdin null, stdout `console.log`, stderr `firecracker.log`, in the worker's process group (no `process_group(0)`: it must stay in `worker_pgid`). Drive order fixes the guest names `vda`, `vdb`, `vdc`. The inspector's `vm.json` is identical except `workspace` is attached from `<work_root>/<task>/ws.img` as well (read-write for journal replay; the agent remounts it read-only before answering) and `uds_path`/`scratch.img` live under `<home>/inspect/<task>/<uuid>/`.

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
2. Preflight (as above); take `ws.lock`; create `<home>/inspect/<task>/<uuid>/` with `vm.json` and `scratch.img`; spawn Firecracker in a **new process group of the controller**, `kill_on_drop`; connect; `Hello{mode: inspect}`; one query; `Shutdown`; wait for exit ≤ 5 s else SIGKILL the group; remove the directory on success (kept, with `console.log`, on failure for diagnosis).
3. The whole call is bounded by `INSPECT_TIMEOUT` = 60 s; on timeout, launch failure or protocol failure: `current_workspace` ⇒ `Some(Err("workspace inspection failed: …"))`; `reconcile` ⇒ `Reconciliation::Unknown` (⇒ unreconcilable, exactly 3a's answer when the host cannot tell).
4. `Reconciliation::Applied` rebuilds the patch-result bytes with the same host code as a live `PatchApplied`, and `SupervisedExecutor::reconcile` retains it as a job receipt as today.

`current_workspace` stays synchronous on the `Executor` trait; the inspector blocks the controller for one boot (about 1–2 s measured for kernel boot plus agent start). Safety argument: an inspector only ever runs when every attempt of the task is settled (3a: `wait_for_job`, `fence_job`, `settled`), so `ws.img` has no other writer; the inspector itself writes nothing but the ext4 journal replay; its VM dies with the controller through the connection-EOF rule. Nothing about this moves into the guest's trust: the digest the inspector reports is computed by our agent from the image, the same way a job computes it.

### Attempt token and handles

The controller mints `attempt_token` (16 random bytes, 32 lowercase hex, `getrandom`) per job in `SupervisedExecutor::job_request` and writes it into `request.json`; the worker passes it in `Hello` over vsock, never on the kernel command line or on a drive. The guest binds the token to the attempt in `Hello` and refuses any other token for the life of the VM. Purpose: job identity (a stray connection to the wrong `v.sock`, an inspector reaching a job VM, a replayed `Hello`) — not authority. It is not a capability handle, it grants nothing, and it is readable by the same UID like everything else in the job directory (3a's stated position).

**No capability handle enters the guest.** Authority is decided on the host: the broker authorizes at intent and re-authorizes at dispatch (3a); the guest can only answer the one request the host sends it; its answer is accepted only through the receipt rules (`valid_outcome` in the supervisor, `check_outcome` and `accept_receipt` in the controller). Revocation mid-job is the 3a path unchanged: `revoke` drops `cancel`, the supervisor SIGKILLs the worker group (worker and Firecracker), the kill-receipt rule applies (a verification killed ⇒ `Failure("cancelled")`; a patch killed ⇒ no receipt ⇒ inspection reconciles it). The 3a note that "wrong-task, wrong-operation and unknown-handle denials become reachable when 3b hands a handle to the guest" is resolved as: those denials stay reachable only through the store's own paths; the guest is not a broker client in v0.1. If a later phase gives the guest something to present a handle to (Phase 4 puts the model broker on the host, so not then), the token slot in `Hello` is where it goes (open question 4).

## Lifecycle and failure modes

### Job VM state machine (worker process)

```
Preflight ──fail──▶ Failure("firecracker worker unavailable: …")              [outcome written]
   │
   ▼
Prepare: ws.lock, ws.img (ReadSnapshot creates), scratch.img, vm.json
   │ fail ─▶ Failure("cannot prepare the VM: …")                               [outcome written]
   ▼
Spawn firecracker ──spawn error──▶ Failure("cannot start firecracker: …")       [outcome written]
   │
   ▼
Booting: wait v.sock, CONNECT/OK, Hello/Ready   (≤ BOOT_TIMEOUT 15 s)
   │ timeout / firecracker exited ─▶ kill VM; Failure("guest did not come up: …") [outcome written: no request was sent, so nothing changed]
   ▼
Serving: send the one request, stream files, await the reply
   │ Refused ─────────────────────▶ Failure(reason)                              [outcome written]
   │ reply ──────────────────────▶ Done
   │ EOF / protocol error / firecracker exited:
   │     Retry kinds (ReadSnapshot, RunVerification) ─▶ Failure("guest exited before reporting: firecracker exit code N")  [outcome written]
   │     ApplyPatch ─────────────▶ NO outcome; worker exits 1  ⇒ supervisor writes no receipt ⇒ reconciled (inspection)
   ▼
Done: Shutdown → wait exit ≤ 5 s else SIGKILL → remove scratch.img → release ws.lock → write outcome.json → exit 0
```

At any point the supervisor may kill the worker group (lease, deadline, cancel): Firecracker dies with the worker; the 3a kill-receipt rule applies unchanged (a valid `outcome.json` present is published; else `Retry` kinds get a `Failure` receipt with the reason; `ReconcileThenRetry` kinds get none). A supervisor SIGKILL leaves the job dead by the lock; the controller's fence kills the session (worker and Firecracker) before `settled()` lets anyone reconcile or redispatch; the guest additionally shuts itself down on connection EOF.

### Failure mapping

| Event | Retry kinds | ApplyPatch |
| --- | --- | --- |
| Preflight, prepare, spawn, boot timeout | `Failure` with the reason | `Failure` (nothing was sent to the guest) |
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
| Host-side Firecracker process (its own heap, vmm threads) | not capped in 3b-1 (needs cgroups ⇒ jailer, 3b-2) | measured at about 30 MiB over `mem_size_mib` in the smoke test's order of magnitude; the KVM tier asserts `VmRSS ≤ mem_size_mib + 96 MiB` |

Concurrency bound: one driver per home (`driver.lock`) means at most **one job VM plus one inspector VM** per home at any time, so a home cannot fan out VMs.

## Security model

Trusted computing base: the host kernel and KVM, Firecracker v1.17.0 with its default seccomp filter, the guest kernel (pinned CI artifact), the guest image bytes (pinned, registered read-only, re-digested before every launch), the `agentos-guest` agent and `git`/`python3` inside the guest, the controller, supervisor and worker. Untrusted: the repository snapshot, the patch, the verification check's behaviour (the profile bytes are trusted by pin, what the check does at run time is not), and anything the guest kernel does after untrusted code runs.

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

Known limits of the security model (kept honest in the README): a VM escape (Firecracker or KVM bug) lands on the host as the controller's UID; micro-architectural side channels are not mitigated beyond `smt: false` per VM (host SMT stays on); the Firecracker process runs as the controller's UID without a chroot, pid namespace or cgroup (3b-2); the host kernel (7.2) is newer than Firecracker's validated hosts; the guest agent is in the TCB; the job directory, `ws.img` and `request.json` are readable and writable by the same UID (3a's position); the guest's clock and entropy come from the host as any VM's do.

## Evidence and export compatibility

- Artifact bytes (`snapshot-manifest`, `patch-result`, `verification-evidence`, `effect-failure`) are produced by the same host functions as today from structured guest replies; `patch.diff`, `patches/`, `evidence/` and the manifest are unchanged for the host worker **byte for byte** (the CLI tests' `normalized()` comparison of crashed vs uncrashed bundles keeps passing for both workers).
- The manifest gains one optional field, `guest_image_digest: Option<Digest>` with `skip_serializing_if = "Option::is_none"`, so host-worker manifests do not change at all and Firecracker manifests name the image that produced the evidence. `status` prints `worker` and, for Firecracker, `guest_image`.
- Evidence from the Firecracker worker contains guest paths (`/workspace`, `/scratch/profile`), never host paths: the README limit "host paths can leak into exports" is narrowed to the host worker.
- `workspace_digest` values are identical across workers for the same tree (shared implementation; golden test: the fixture snapshot digest `be77aa19c032f85329a9596adfd692252a0c87fd09d337b1873feb6003bdd3b8` and profile digest `9ff584f31b7fef8ac5774ced5c8f1620c27f736b4bdc4d9e03e553df5d8ea12c` from the README must still be produced by `agentos_core::workspace::workspace_digest`, and the KVM tier asserts the guest's `SnapshotDone.workspace_digest` equals the host-computed digest of the staged snapshot).
- The crash matrix (`crates/agentos-engine/tests/crash_matrix.rs`) and the CLI crash demo table (`DEMO` in `crates/agentos-cli/tests/cli.rs`, 10 rows) run against the Firecracker worker in the KVM tier with the **same expected decisions, execution counts and lease generations** as 3a; `during-execute:apply_patch` still resumes by `PublishRetained`, and the exit-before-receipt rows by `PublishReconciled` (now through an inspection boot) and `Redispatch`.

## Testing

Tiers; every tier runs under `docker compose`.

**T0 — pure unit (`docker compose run --rm test cargo test --workspace`, no KVM):** frame encode/decode and every limit (boundary ±1); message serde round trips; `vm.json` rendering against a golden file (paths, no network, `smt: false`, drive order); exit-code mapping; `FirecrackerConfig` validation (relative paths, `vcpus` 0/33, `memory_mib` 127); token format and per-job freshness; image registry (register, dedupe, `@`/traversal refused, read-only bits); contract `guest_image_digest` optional and byte-identical serialization without it; `agentos_core::workspace` golden digests; guest-side request validation (the `Refused` reason table) exercised in-process against directories.

**T1 — fake guest (same command, no KVM):** `agentos-guest --fake <uds> <root_dir>` runs the real agent code as a host process: it listens on a Unix socket, implements Firecracker's `CONNECT <port>\n` / `OK <port>\n` handshake, and uses `<root_dir>/{workspace,scratch}` directories instead of block devices (no `mkfs`, no uid switch, no mounts — those are the KVM tier's job). `FirecrackerConfig.launcher = Fake` (honoured only with `AGENTOS_TEST_WORKERS=1`, passed with `Command::env`, never `set_var`) makes `FirecrackerWorker` and `Inspector` spawn it instead of Firecracker. On this seam:

- **Worker conformance suite** (`crates/agentos-engine/tests/worker_conformance.rs`): one table of cases run against `HostProcessWorker`, `FirecrackerWorker+Fake`, and (T2) `FirecrackerWorker+Real`: snapshot digest equals the host digest; patch applies and the result bytes equal the host worker's; version conflict; non-editable path; traversal; symlink; excluded component; binary patch; verification passes/fails with identical evidence fields; pinned-profile mismatch; `timeout`; oversized output truncation flags; `reconcile` answers `NotApplied`/`Applied`/`Unknown` on a base, a patched and a tampered workspace; `current_workspace` on a missing workspace. Outcomes are compared field by field after normalizing nothing: the bytes must be equal.
- Kill paths through the real supervisor binary: lease expiry kills the worker and the fake guest process; cancel marker; deadline; supervisor SIGKILL then fence settles the job and no process of it remains; EOF on the control connection shuts the fake guest down within 500 ms.
- Failure mapping table: boot timeout (fake guest told to never listen), EOF after `ApplyPatch` was sent ⇒ no outcome ⇒ the supervisor writes no receipt ⇒ `SupervisedExecutor::run` reconciles through the fake inspector; EOF after `RunVerification` ⇒ `Failure("guest exited before reporting…")`; a second `Hello` with another token is refused; a frame over the limit is a protocol failure; `ws.lock` held ⇒ the stated outcomes.
- The crash matrix and the CLI crash demo table run with `AGENTOS_TEST_WORKER=firecracker-fake` (the engine's `common::supervised` and the CLI's `Cli::cmd` honour it) with unchanged expectations.

**T2 — KVM-gated (`docker compose run --rm test-kvm cargo test --workspace`; the tests gate on `AGENTOS_KVM_TESTS`, not on `#[ignore]`):** the `test-kvm` service is the `test` service plus `devices: ["/dev/kvm:/dev/kvm"]`, `AGENTOS_KVM_TESTS=1`, the `guest-images` volume and `scripts/fetch-firecracker.sh` output. KVM tests begin with `let Some(kvm) = kvm::require() else { return };` where `require()` returns `None` and prints `SKIPPED: set AGENTOS_KVM_TESTS=1 and pass /dev/kvm (docker compose run --rm test-kvm …)` when the variable is unset, and **panics** when it is set but `/dev/kvm`, the binary or the image is missing, so a KVM-expected environment can never pass silently. T2 contents: the conformance suite on the real guest; the threat-model table above; image build reproducibility (`build-guest-image.sh --verify`); boot time budget (`Ready` within 5 s on this host, asserted ≤ `BOOT_TIMEOUT`); the crash matrix and CLI demo table on the real worker; `scripts/demo.sh --worker firecracker` producing the README transcript; inspection after a killed patch VM (test hook `AGENTOS_TEST_KILL_VM_AFTER_REQUEST=1`, honoured only with `AGENTOS_TEST_WORKERS=1`: the worker SIGKILLs Firecracker right after sending `ApplyPatch`, so the image holds either the base or the base plus the patch; the worker writes no outcome, the supervisor no receipt, and recovery reconciles by inspection with the decision `PublishReconciled` or `Redispatch` according to what the image holds; the task converges to SUCCEEDED and the journal's digest equals the inspector's); `current_workspace` on resume boots an inspector and the task continues; concurrency bound (never more than two Firecracker processes under the home during the whole demo).

What is verified only with KVM: real boot, `mkfs`/mount, uid separation, OOM behaviour, CPU and memory bounds, the no-network guarantee, host-secret isolation, squashfs read-only root, journal replay in the inspector. Everything else (protocol, worker, inspector plumbing, kill paths, outcome bytes, recovery decisions) is verified in T1 on every `cargo test --workspace`.

Flakiness control: as in 3a, the supervisor and crash-matrix suites run 20× in one container before a task is done; the KVM tier 10×; leases in kill tests are hundreds of milliseconds with real clocks; boot timeouts are generous (15 s) relative to the measured 1–2 s; the kernel download and the Firecracker tarball are verified by sha256 and cached in compose volumes; the image build is deterministic and cached by digest; no test depends on `v.sock` appearing within a fixed time other than `BOOT_TIMEOUT`.

## Acceptance for 3b-1

The build plan's Phase 3 rows left open by 3a: "guest attempts network access or host secret access — the reference worker configuration provides neither resource" (threat-model rows 1–2 above, KVM tier) and "repository code stays within its workspace and configured resources" (rows 3–6; workspace containment by uid and mount layout). Plus: every 3a acceptance row still passes against the Firecracker worker (crash matrix, CLI demo table, revoke mid-job stops the VM, deadline kills the VM, duplicate/late receipts); the conformance suite shows the two workers are observationally equal on the fixture; `cargo test --workspace` stays green without KVM; the README transcript for the Firecracker demo is real output.

## Rollout and README

- Default worker stays `host`; `--worker firecracker` opts in per task; the recorded worker drives later commands. `scripts/demo.sh` gains `--worker firecracker` and `scripts/fetch-firecracker.sh` plus `scripts/build-guest-image.sh` are documented in "Build and test" with the `test-kvm` service.
- README "Known limits": **"Not sandboxed"** becomes "Sandboxed only with `--worker firecracker`" (host worker: unchanged text); **"`reconcile` runs in the controller"** becomes "inspection boots a VM from the controller process (bounded, read-only on the workspace); a missing `/dev/kvm` at resume fails the command before the task is touched"; **"Host paths can leak into exports"** narrowed to the host worker; add: "Firecracker runs unjailed as the controller's UID (3b-2 adds the jailer)", "workspace and scratch images are not garbage-collected", "the host kernel is newer than Firecracker's validated hosts", "one VM boot per effect (no VM reuse)". The "Phase 3b (not built yet)" section is replaced by a "Phase 3b-2 (not built yet)" list.

## Known limits (after 3b-1)

- Same-UID Firecracker process, no chroot/pid-ns/cgroup for it; the controller's UID is the blast radius of a VM escape.
- `ws.img` and `scratch.img` sizes are constants (1 GiB / 512 MiB), not contract limits; images are never collected.
- One boot per effect (1–2 s each) and one inspector boot per resume and per receipt-less patch; no VM reuse or snapshots.
- The guest image build needs network access (snapshot.debian.org, the Firecracker CI bucket) and root in the build container; the kernel is a pinned download, not built from source.
- The preflight refuses a command (exit 1, task untouched) when KVM, the binary or the image is unusable; an inspector that fails after the preflight passed (the device vanished, a boot failure) fails the task on resume with `workspace lost: workspace inspection failed: …`. Rare, but a task can fail for an infrastructure reason.
- No aarch64; x86_64 only (the image recipe pins the x86_64 kernel).
- The guest trusts the host's time (`kvm-clock`) and the serial console is the only guest log.

## Risks and open questions (owner decisions)

1. **Guest base distribution.** Recommendation: Debian bookworm from `snapshot.debian.org` (exact reproducibility, glibc Python). Alternative: Alpine (smaller image, no snapshot archive, musl Python). Designed for Debian.
2. **Disk image sizes.** Recommendation: constants 1 GiB / 512 MiB in 3b-1; `worker_disk_mib` in the contract in 3b-2 (optional field, `skip_serializing_if`, so digests of existing contracts stay). Designed as constants.
3. **Jailer in 3b-1 when running as root.** Recommendation: no; 3b-2. The dev setup has neither root on the host nor a writable cgroup fs in the container, so it would be untested code. Designed without.
4. **A capability handle in the guest.** Recommendation: none in v0.1 (nothing in the guest can present it; it would be a bearer secret readable by untrusted code). The `Hello` message keeps the token slot. Designed without.
5. **Inspector run by the controller process vs as a supervised job.** Recommendation: controller process (read-only use, bounded 60 s, dies with the controller through connection EOF, no job-directory churn). Designed that way.
6. **Guest kernel.** Recommendation: the CI `vmlinux-6.18.51` artifact pinned by sha256 (supported to 2028; booted here). Building from `resources/guest_configs` is 3b-2/Phase 6. Designed for the artifact.
7. **Host kernel 7.2 is not in Firecracker's validated list.** Recommendation: accept (smoke test passed); record the host kernel version in `Submitted` for Firecracker tasks so a later failure is attributable. Designed with that field.
8. **Cache mode for `ws.img`.** Recommendation: `Writeback` (guest `fsync` reaches the host file; needed for the reconcile argument) at a small write-performance cost. Designed with `Writeback`.

## Self-review

- Placeholder scan: no TBD/TODO; every "later" item is assigned to 3b-2 or an open question with a designed default.
- Contradictions checked: Firecracker is in the worker's group (not `process_group(0)`), so the 3a `worker_pgid` kill covers it, and `groups` needs no entry; the inspector attaches `ws.img` read-write only for journal replay and remounts read-only, consistent with "the inspector writes nothing but the journal replay"; `ApplyPatch` never produces a `Failure` after its request was sent (unresolved or no outcome), consistent with the 3a kill-receipt rule; manifests change only by one optional field; the contract schema gains only an optional, non-serialized-when-absent pin.
- Scope: 3b-1 is the worker, image, protocol, inspection, selection and tests; 3b-2 is hardening of the host-side process and sizes. The first plan covers 3b-1.
- Ambiguities resolved inline: which process spawns Firecracker, where every file lives, every limit and timeout as a number, every reason string, every exit-code mapping, who writes which byte of an outcome.
