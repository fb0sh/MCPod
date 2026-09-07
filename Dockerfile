# syntax=docker/dockerfile:1

# ---------- Stage 1: build the MCP server (§7) ----------
FROM rust:1-slim-bookworm AS builder

WORKDIR /app

# Cache the dependency build separately from our own crate.
COPY mcp-server/Cargo.toml mcp-server/Cargo.lock ./
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    mkdir -p src && echo 'fn main() {}' > src/main.rs && \
    cargo build --release && rm -rf src

COPY mcp-server/src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    touch src/main.rs && cargo build --release && \
    cp target/release/mcpod /usr/local/bin/mcpod

# ---------- Stage 2: Debian 13 runtime (§6) ----------
FROM debian:13-slim

# §2.1 base toolset: shell, VCS, transfers, build toolchain, inspection, ssh, process tools.
# Python build deps (libffi/openssl/zlib/...) let mise-compiled interpreters
# and pip source builds work out of the box.
RUN apt-get update && apt-get install -y --no-install-recommends \
        bash \
        git \
        curl \
        wget \
        ca-certificates \
        build-essential \
        pkg-config \
        cmake \
        ninja-build \
        jq \
        ripgrep \
        fd-find \
        tree \
        file \
        less \
        patch \
        diffutils \
        rsync \
        tar \
        gzip \
        bzip2 \
        xz-utils \
        unzip \
        zip \
        openssh-client \
        gnupg \
        procps \
        psmisc \
        lsof \
        strace \
        iproute2 \
        netcat-openbsd \
        sudo \
        libffi-dev \
        libssl-dev \
        zlib1g-dev \
        libbz2-dev \
        liblzma-dev \
        libreadline-dev \
        libsqlite3-dev \
        libncursesw5-dev \
        tk-dev \
        libxml2-dev \
        libxmlsec1-dev \
        libgdbm-dev \
        libnss3-dev \
        libxslt1-dev \
    && ln -sf /usr/bin/fdfind /usr/local/bin/fd \
    && rm -rf /var/lib/apt/lists/*

# mise manages development runtimes (§8): toolchains come from mise, not apt.
# A global python is preinstalled so agents can run scripts immediately;
# projects can pin their own version via mise.toml.
ENV MISE_INSTALL_PATH=/usr/local/bin/mise
RUN curl -fsSL https://mise.run | sh
ENV MISE_DATA_DIR=/usr/local/share/mise \
    MISE_GLOBAL_CONFIG_FILE=/usr/local/share/mise/mise.toml \
    PATH="/usr/local/share/mise/shims:${PATH}"
RUN mise use -g python@3.10

# Common libraries for agent scripts, installed into the mise-managed global
# python via its pip. reshim afterwards so pip-installed entry points
# (ruff/black/pytest/...) get their own shims.
RUN mise exec python -- python -m pip install --no-cache-dir --upgrade pip \
    && mise exec python -- python -m pip install --no-cache-dir \
        requests \
        httpx \
        aiohttp \
        pyyaml \
        toml \
        jsonschema \
        python-dateutil \
        pytz \
        regex \
        beautifulsoup4 \
        lxml \
        html5lib \
        orjson \
        msgpack \
        protobuf \
        pandas \
        numpy \
        rich \
        tqdm \
        pytest \
        black \
        ruff \
        mypy \
        virtualenv \
        pipx \
    && mise reshim

COPY --from=builder /usr/local/bin/mcpod /usr/local/bin/mcpod

# Login shells (bash -l) reset PATH from /etc/profile; keep the mise shims
# first so `python`/`ruff`/`pytest` resolve everywhere.
RUN printf 'export PATH="/usr/local/share/mise/shims:$PATH"\n' > /etc/profile.d/mise-shims.sh

WORKDIR /workspace
ENV MCPOD_HOST=0.0.0.0 \
    MCPOD_PORT=3000 \
    MCPOD_WORKSPACE=/workspace
EXPOSE 3000

HEALTHCHECK --interval=30s --timeout=3s --start-period=2s --retries=3 \
    CMD curl -fsS "http://localhost:${MCPOD_PORT}/health" || exit 1

ENTRYPOINT ["/usr/local/bin/mcpod"]
