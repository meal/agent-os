# Kernel provenance and fresh-host installation — design

Status: design for package 11 of the [v0.1 completion plan](../plans/2026-10-04-v01-completion.md).
Parent: [v0.1 completion design, "Milestone C"](2026-10-04-v01-completion-design.md).

## Goal

The guest kernel is built from pinned source with a pinned configuration and toolchain, twice
in independent build trees, with identical output and recorded provenance. A release can be
installed on a supported host by one script that verifies checksums, stages atomically,
never overwrites an existing home, is idempotent per version, and refuses an unsuitable host
with an actionable reason. A smoke run proves the installed release works end to end.

## Kernel from source

Today every recipe downloads Firecracker CI's prebuilt `vmlinux-6.18.51` by sha256. Registered
images are immutable, so the source kernel goes into a new recipe, `python-stdlib-py314-v2`:
the py314 candidate with the kernel built here. Earlier recipes keep their downloaded kernel.

- **Source.** kernel.org `linux-6.18.51.tar.xz`, sha256
  `ba2f60f858bf4d1f929101faa356c93dc8b925b17aaa9f95eabd4627758df613` (from kernel.org's
  `sha256sums.asc`, checked 2026-10-09), cached under `build/kernels/` and verified on every
  use.
- **Configuration.** Firecracker v1.17.0's own fragments, committed under
  `guest/kernel/firecracker-v1.17.0/` with their sha256: `microvm-kernel-ci-x86_64-6.18.config`
  (`ba22401a…cf5215`), `ci.config` (`6ce47107…7f47e`) and `nvme.config`. They are
  concatenated in Firecracker's order and resolved with `make olddefconfig`, as its
  `rebuild.sh` does. Firecracker builds these from Amazon Linux kernel tags with patches;
  this build uses upstream source, so the resolved `.config` is committed too and a test
  fails if `olddefconfig` ever resolves it differently.
- **Toolchain.** A dedicated `kernel-builder` Compose service from `debian:bookworm-slim`
  pinned by image digest, with `gcc`, `make`, `bc`, `flex`, `bison`, `libelf-dev`,
  `libssl-dev` and `dwarves` from the same pinned `snapshot.debian.org` timestamp the guest
  rootfs uses. The compiler and binutils versions are recorded.
- **Reproducibility.** `KBUILD_BUILD_TIMESTAMP` from the recipe's snapshot timestamp,
  `KBUILD_BUILD_USER=agentos`, `KBUILD_BUILD_HOST=agentos`, `KBUILD_BUILD_VERSION=1`, an
  empty `LOCALVERSION`, the build tree always at `/build/linux-6.18.51` inside the
  container, and `-j` not affecting output. `--verify` builds twice in two fresh volumes and
  compares `vmlinux` byte for byte, then builds the image twice as today.
- **Provenance.** `image.json` gains `"kernel": {"version", "source_sha256", "config_sha256",
  "gcc", "binutils"}` next to the interpreter record. A KVM test boots the image and compares
  `/proc/version` and the embedded config (`CONFIG_IKCONFIG_PROC`) with the record.

## Release layout

`scripts/release.sh VERSION` (used here for the smoke run, published by package 12) produces
`agentos-VERSION-x86_64-linux.tar.gz` and its `.sha256`, containing:

```
agentos-VERSION-x86_64-linux/
  bin/agentos              static musl build
  bin/firecracker, bin/jailer   v1.17.0, as fetched and checked today
  images/<id>/             image.json, vmlinux, rootfs.squashfs
  profiles/parser-checks-v1/
  components/repo-analyzer-v1/
  MANIFEST.json            version, commit, every file's sha256, image/profile/component digests
  SHA256SUMS
```

## Installer (`scripts/install.sh`)

A POSIX `sh` script with no dependency beyond coreutils, `tar` and `sha256sum`:

```
install.sh TARBALL --sha256 HEX [--prefix DIR] [--home DIR] [--allow-unjailed]
```

Defaults: prefix `$HOME/.local/share/agentos`, home `$HOME/.agentos`. In order, each refusal
exiting 2 with a reason and leaving nothing behind:

1. **Architecture.** `uname -m` must be `x86_64`.
2. **Checksum.** The tarball's sha256 must be `--sha256`; after extraction into a staging
   directory, every file must match `SHA256SUMS`, and `SHA256SUMS` must list every file.
3. **KVM.** `/dev/kvm` must exist and be readable and writable by the user.
4. **cgroups.** A cgroup v2 hierarchy with `cpu`, `memory` and `pids` delegable, unless
   `--allow-unjailed`, which is recorded and warned about, as the CLI does.
5. **Staging.** Extraction goes to `PREFIX/.staging-VERSION-<pid>`; any older staging
   directory from an interrupted run is removed first. The staged tree is moved into place
   with one `rename` to `PREFIX/VERSION`, and `PREFIX/current` is switched with a symlink
   rename. Same version with identical checksums: a no-op. Same version with different
   content: refused.
6. **Home.** Created only when absent; an existing home is never modified except by
   registering the release's image, profile and component, which is idempotent and
   content-addressed.

"Docker-based" means the installer's tests and the smoke run execute in fresh containers;
the installer itself runs directly on the host.

## Smoke run (`scripts/smoke-install.sh`)

Builds a release (or takes one), starts a fresh `debian:bookworm-slim` container (pinned by
digest) with `/dev/kvm` and the same capabilities as `test-kvm`, installs the release as an
unprivileged user and as root (jailed), registers the image, profile and component, submits
the parser fixture with the analyzer on the jailed worker with a crash during the patch,
resumes it, exports it, and compares the exported patch with `fixtures/parser-repo.fix.patch`
applied to the recorded snapshot and the evidence profile digest with the registered one.
Only one KVM host is available, so a fresh container on it stands in for a fresh host; the
evidence says so.

## Tests

| Test | Tier |
| --- | --- |
| Kernel built twice in independent volumes is byte-identical; the committed resolved config is what `olddefconfig` produces | KVM host (needs the builder service; no KVM needed) |
| The booted kernel's version and config match `image.json` | real KVM |
| The whole KVM suite on `python-stdlib-py314-v2` | real KVM |
| `install.sh --self-test`: corrupted tarball, missing or extra file in `SHA256SUMS`, existing home kept, interrupted staging cleaned, unsupported architecture, missing KVM, cgroups not delegable; after each refusal nothing new exists | default |
| Idempotent reinstall; same version with different content refused | default |
| `smoke-install.sh` end to end | real KVM |

## Out of scope

Signing releases, other architectures, distribution packages, and changing the default image.
