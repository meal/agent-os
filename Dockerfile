FROM rust:1.98.1-bookworm
RUN rustup component add clippy rustfmt
# mmdebstrap, squashfs-tools: the guest image (scripts/build-guest-image.sh); musl-tools and
# the musl target: the static agentos-guest; curl: Firecracker and the guest kernel.
RUN apt-get update && apt-get install -y python3 git mmdebstrap squashfs-tools musl-tools curl ca-certificates && rm -rf /var/lib/apt/lists/*
RUN rustup target add x86_64-unknown-linux-musl
ENV CC_x86_64_unknown_linux_musl=musl-gcc
WORKDIR /work
