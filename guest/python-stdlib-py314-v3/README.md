# Python 3.14, protocol 2 (the release image)

The `python-stdlib-py314-v2` recipe (Python 3.14.8, linux 6.18.51 built from pinned source) at
guest protocol 2. It contains **no agent CLI**: this is the image a published release ships.
Build the kernel first, then the image:

```sh
docker compose run --rm kernel-builder sh scripts/build-kernel.sh build/kernels/out --verify
docker compose run --rm test-kvm sh scripts/build-guest-image.sh guest/python-stdlib-py314-v3 build/guest-images/python-stdlib-py314-v3 --verify
```

`hooks/customize.sh`, `kernel.lock`, `snapshot.lock` and `packages.txt` are the v2 recipe's,
unchanged; only the manifest differs (`"protocol":2`, the id and `built_from`).

To run an agent CLI in the guest, build `guest/agent-cli-py314-v1` instead: this recipe plus the
pinned Claude Code binary, which that build downloads from npm and checks against
`agent-cli.lock` on your machine. The binary is Anthropic's ("All rights reserved", used under
Anthropic's terms), so no release ships it.
