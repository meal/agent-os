# Current Python candidate

This separate recipe uses the existing pinned Debian snapshot and kernel and installs
the pyenv-managed Python 3.14.8 built by the development Docker image. It does not replace
python-stdlib-v1. Its manifest records interpreter source and pyenv commit provenance.

Build with the reviewed Dockerfile, then run the image builder with this recipe and
`--verify`. Two-image reproducibility and real KVM conformance remain required before
using it as the alpha default. Source/toolchain reproducibility is completed in the
kernel/packaging work package; copying the pinned interpreter does not prove two
independent interpreter source builds are identical.
