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
        util-linux \
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

# mcpod: the identity the MCP server runs as. The entrypoint remaps this
# user onto the workspace owner's UID/GID at container start
# (scripts/docker-entrypoint.sh); 1000 is just a build-time placeholder.
# Passwordless sudo lets agents run system-level operations
# (`sudo apt-get install -y <pkg>`); files created via sudo are root-owned.
RUN groupadd -g 1000 mcpod \
    && useradd -m -u 1000 -g mcpod -s /bin/bash mcpod \
    && echo 'mcpod ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/mcpod \
    && chmod 0440 /etc/sudoers.d/mcpod

# mise manages development runtimes (§8): toolchains come from mise, not apt.
# A global python is preinstalled so agents can run scripts immediately;
# projects can pin their own version via mise.toml.
# Build-time layout: shared system runtimes under /usr/local/share/mise,
# installed as root and read-only for the runtime user.
# (Download to a file first: `curl | sh` pipelines mask download failures.)
ENV MISE_INSTALL_PATH=/usr/local/bin/mise
RUN curl -fsSL https://mise.run -o /tmp/mise-install.sh \
    && sh /tmp/mise-install.sh \
    && rm -f /tmp/mise-install.sh \
    && test -x /usr/local/bin/mise
ENV MISE_DATA_DIR=/usr/local/share/mise \
    MISE_GLOBAL_CONFIG_FILE=/usr/local/share/mise/mise.toml
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

# Runtime layout: mcpod's mise state (new installs, cache, global config)
# lives in its HOME so `mise install` works as an unprivileged user; the
# preinstalled system runtimes above stay shared read-only (the entrypoint
# exposes them to mcpod through per-version symlinks). User shims take
# precedence, system shims remain as fallback.
ENV MISE_DATA_DIR=/home/mcpod/.local/share/mise \
    MISE_GLOBAL_CONFIG_FILE=/home/mcpod/.config/mise/config.toml \
    PATH="/home/mcpod/.local/share/mise/shims:/usr/local/share/mise/shims:${PATH}"

COPY --from=builder /usr/local/bin/mcpod /usr/local/bin/mcpod

# Login shells (bash -l) reset PATH from /etc/profile; keep both mise shim
# dirs (user-level first) so `python`/`ruff`/`pytest` resolve everywhere.
RUN printf 'export PATH="/home/mcpod/.local/share/mise/shims:/usr/local/share/mise/shims:$PATH"\n' > /etc/profile.d/mise-shims.sh

# Entrypoint: map mcpod -> workspace owner UID/GID, set up $HOME, drop
# privileges, then exec the server (PID 1 = mcpod). chmod 0755 (not +x) so
# the mode is independent of the build host's umask: a --user started
# container must still be able to read and execute the script.
COPY scripts/docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod 0755 /usr/local/bin/docker-entrypoint.sh

WORKDIR /workspace
ENV MCPOD_HOST=0.0.0.0 \
    MCPOD_PORT=3000 \
    MCPOD_WORKSPACE=/workspace
EXPOSE 3000

HEALTHCHECK --interval=30s --timeout=3s --start-period=2s --retries=3 \
    CMD curl -fsS "http://localhost:${MCPOD_PORT}/health" || exit 1

ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
