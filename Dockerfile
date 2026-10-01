FROM rust:1.98-bookworm
RUN apt-get update && apt-get install -y python3 git && rm -rf /var/lib/apt/lists/*
WORKDIR /work
