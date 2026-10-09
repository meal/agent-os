# Current Python candidate

This separate recipe uses the existing pinned Debian snapshot and kernel and installs
the pyenv-managed Python 3.14.8 built by the development Docker image. It does not replace
python-stdlib-v1. Its manifest records interpreter source and pyenv commit provenance.

Build with the reviewed Dockerfile, then run the image builder with this recipe and
`--verify`. Two-image reproducibility and real KVM conformance remain required before
using it as the alpha default. Source/toolchain reproducibility is completed in the
kernel/packaging work package; copying the pinned interpreter does not prove two
independent interpreter source builds are identical.

## Protocol 2

The guest protocol is now 2 (`GUEST_PROTOCOL` in `crates/agentos-core/src/guest.rs`). Images built before this change say `"protocol":1` and a v2 controller refuses them: they must be rebuilt. `image.json.in` still says `"protocol":1` and is not changed here; it has to move to 2 together with the rebuild, or the new image is refused too.
