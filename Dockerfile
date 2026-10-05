FROM rust:1.98.1-bookworm AS development
RUN rustup component add clippy rustfmt
# mmdebstrap, squashfs-tools: the guest image (scripts/build-guest-image.sh); musl-tools and
# the musl target: the static agentos-guest; curl: Firecracker and the guest kernel.
RUN apt-get update && apt-get install -y python3 git mmdebstrap squashfs-tools musl-tools curl ca-certificates && rm -rf /var/lib/apt/lists/*
RUN apt-get update && apt-get install -y libssl-dev libbz2-dev libreadline-dev libsqlite3-dev libffi-dev liblzma-dev zlib1g-dev libncurses-dev tk-dev xz-utils libzstd-dev && rm -rf /var/lib/apt/lists/*
ENV PYENV_ROOT=/opt/pyenv
ENV PATH="/opt/pyenv/shims:/opt/pyenv/bin:${PATH}"
COPY runtime /opt/agentos-runtime
RUN . /opt/agentos-runtime/python.lock && \
    curl -fsSL --proto '=https' --proto-redir '=https' "https://codeload.github.com/pyenv/pyenv/tar.gz/$AGENTOS_PYENV_COMMIT" -o /tmp/pyenv.tar.gz && \
    echo "$AGENTOS_PYENV_SHA256  /tmp/pyenv.tar.gz" | sha256sum -c - && \
    mkdir -p "$PYENV_ROOT" && tar -xzf /tmp/pyenv.tar.gz --strip-components=1 -C "$PYENV_ROOT" && \
    cp /opt/agentos-runtime/python-3.14.8 "$PYENV_ROOT/plugins/python-build/share/python-build/$AGENTOS_PYTHON_VERSION" && \
    MAKE_OPTS=-j2 pyenv install "$AGENTOS_PYTHON_VERSION" && pyenv global "$AGENTOS_PYTHON_VERSION" && \
    ln -s "$PYENV_ROOT/versions/$AGENTOS_PYTHON_VERSION/bin/python3" /usr/local/bin/python3 && \
    ln -s "$PYENV_ROOT/versions/$AGENTOS_PYTHON_VERSION/bin/python" /usr/local/bin/python && \
    rm /tmp/pyenv.tar.gz
# Verification inherits PATH only. Resolve Python directly, since invoking a pyenv shim
# would add PYENV_* variables. Development can select versions explicitly with pyenv exec.
ENV PATH="/opt/pyenv/bin:/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
RUN rustup target add x86_64-unknown-linux-musl
ENV CC_x86_64_unknown_linux_musl=musl-gcc
WORKDIR /work

# Optional offline UI acceptance environment; production has no browser dependency.
FROM development AS ui-test
COPY runtime/ui-browser-requirements.txt /opt/agentos-runtime/ui-browser-requirements.txt
RUN pyenv exec python -m venv /opt/agentos-ui-venv && \
    /opt/agentos-ui-venv/bin/python -m pip install --no-cache-dir -r /opt/agentos-runtime/ui-browser-requirements.txt && \
    /opt/agentos-ui-venv/bin/python -m playwright install --with-deps chromium
FROM development AS default
