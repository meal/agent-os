# Python 3.14 with a source-built kernel

The `python-stdlib-py314-v1` recipe with the guest kernel built here from pinned source
(`guest/kernel/`, `scripts/build-kernel.sh`) instead of Firecracker CI's prebuilt one. Build
the kernel first, twice for the reproducibility check, then the image:

```sh
docker compose run --rm kernel-builder sh scripts/build-kernel.sh build/kernels/out --verify
docker compose run --rm test-kvm sh scripts/build-guest-image.sh guest/python-stdlib-py314-v2 build/guest-images/python-stdlib-py314-v2 --verify
```

`kernel.lock` pins the vmlinux by sha256; `image.json` records the kernel's source, resolved
configuration and toolchain.

## Protocol 2

The guest protocol is now 2 (`GUEST_PROTOCOL` in `crates/agentos-core/src/guest.rs`). Images built before this change say `"protocol":1` and a v2 controller refuses them: they must be rebuilt. `image.json.in` still says `"protocol":1` and is not changed here; it has to move to 2 together with the rebuild, or the new image is refused too.
