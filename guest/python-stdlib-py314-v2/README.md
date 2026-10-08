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
