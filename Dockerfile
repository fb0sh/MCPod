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
    && ln -sf /usr/bin/fdfind /usr/local/bin/fd \
    && rm -rf /var/lib/apt/lists/*

# mise manages development runtimes (§8); toolchains themselves come from the
# project's mise.toml, not from this image.
ENV MISE_INSTALL_PATH=/usr/local/bin/mise
RUN curl -fsSL https://mise.run | sh
ENV MISE_DATA_DIR=/usr/local/share/mise
ENV PATH="/usr/local/share/mise/shims:${PATH}"

COPY --from=builder /usr/local/bin/mcpod /usr/local/bin/mcpod

WORKDIR /workspace
ENV MCPOD_HOST=0.0.0.0 \
    MCPOD_PORT=3000 \
    MCPOD_WORKSPACE=/workspace
EXPOSE 3000

HEALTHCHECK --interval=30s --timeout=3s --start-period=2s --retries=3 \
    CMD curl -fsS "http://localhost:${MCPOD_PORT}/health" || exit 1

ENTRYPOINT ["/usr/local/bin/mcpod"]
