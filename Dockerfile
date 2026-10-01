FROM rust:1.98.1-bookworm
RUN rustup component add clippy rustfmt
RUN apt-get update && apt-get install -y python3 git && rm -rf /var/lib/apt/lists/*
WORKDIR /work
