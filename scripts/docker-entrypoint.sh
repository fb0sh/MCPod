#!/usr/bin/env bash
# MCPod container entrypoint — the permission model (see README "容器权限模型"):
#
#   1. Read the owner UID/GID of $MCPOD_WORKSPACE (the bind-mounted project).
#   2. Remap the `mcpod` user/group onto that UID/GID (usermod/groupmod -o,
#      tolerating collisions with existing system users/groups).
#   3. Prepare /home/mcpod ($HOME, user-level mise state) for that identity.
#   4. Drop privileges and exec the MCP server, so the whole process tree —
#      server, bash tool children, write/edit atomic renames — runs as the
#      workspace owner and host-side file ownership is preserved.
#
# The `mcpod` user has passwordless sudo (sudoers.d/mcpod from the Dockerfile),
# so agents can install system packages with `sudo apt-get install ...`.
#
# Root-owned workspaces (uid 0: Docker Desktop bind mounts, root-owned
# volumes) keep running as root — the historical behavior.
set -euo pipefail

mcpod_bin=/usr/local/bin/mcpod
workspace="${MCPOD_WORKSPACE:-/workspace}"

# Started with an explicit `docker run --user`: remapping is impossible.
# Docker leaves HOME unset (or "/") for uid values without a passwd entry.
if [ "$(id -u)" != "0" ]; then
    case "${HOME:-/}" in "" | "/") export HOME=/home/mcpod ;; esac
    exec "$mcpod_bin" "$@"
fi

mkdir -p "$workspace"
uid="$(stat -c '%u' "$workspace")"
gid="$(stat -c '%g' "$workspace")"

# ---------------------------------------------------------------------------
# Root-owned workspace: run everything as root (Docker Desktop file sharing,
# root-owned volumes, or an explicit root bind mount).
# ---------------------------------------------------------------------------
if [ "$uid" = "0" ]; then
    echo "mcpod: workspace $workspace is root-owned; running server as root" >&2
    export HOME=/root
    export USER=root LOGNAME=root
    export MISE_DATA_DIR=/usr/local/share/mise
    export MISE_GLOBAL_CONFIG_FILE=/usr/local/share/mise/mise.toml
    exec "$mcpod_bin" "$@"
fi

# ---------------------------------------------------------------------------
# Remap mcpod -> workspace owner. -o tolerates the target uid/gid already
# existing in the image (e.g. gid 100 `users`, uid 33 www-data).
# ---------------------------------------------------------------------------
remapped=0
if [ "$(id -g mcpod)" != "$gid" ]; then
    groupmod -o -g "$gid" mcpod
    remapped=1
fi
if [ "$(id -u mcpod)" != "$uid" ]; then
    usermod -o -u "$uid" mcpod
    remapped=1
    # If another user already occupies this uid, getpwuid() resolves to that
    # name (first /etc/passwd match) and sudo/whoami break. Move the mcpod
    # entry to the top so it wins.
    if [ "$(getent passwd "$uid" | head -n1 | cut -d: -f1)" != "mcpod" ]; then
        grep '^mcpod:' /etc/passwd >/etc/passwd.mcpod
        grep -v '^mcpod:' /etc/passwd >/etc/passwd.rest
        cat /etc/passwd.mcpod /etc/passwd.rest >/etc/passwd.new
        mv /etc/passwd.new /etc/passwd
        rm -f /etc/passwd.mcpod /etc/passwd.rest
    fi
fi

export HOME=/home/mcpod
export USER=mcpod LOGNAME=mcpod
[ -d "$HOME" ] || mkdir -p "$HOME"

# ---------------------------------------------------------------------------
# mise: user-level state (installs/cache/config) lives in $HOME; the
# preinstalled system runtimes under /usr/local/share/mise stay read-only and
# are shared through per-version symlinks into the user data dir. This keeps
# `mise install` writable without ever loosening the system tree's perms.
# ---------------------------------------------------------------------------
user_data="$HOME/.local/share/mise"
mkdir -p "$user_data/installs" "$HOME/.config/mise"
if [ -d /usr/local/share/mise/installs ]; then
    for tool_path in /usr/local/share/mise/installs/*; do
        [ -d "$tool_path" ] || continue
        tool="$(basename "$tool_path")"
        mkdir -p "$user_data/installs/$tool"
        for version_path in "$tool_path"/*; do
            [ -e "$version_path" ] || continue
            version="$(basename "$version_path")"
            [ -e "$user_data/installs/$tool/$version" ] ||
                ln -s "$version_path" "$user_data/installs/$tool/$version"
        done
    done
fi
if [ -f /usr/local/share/mise/mise.toml ] && [ ! -f "$HOME/.config/mise/config.toml" ]; then
    cp /usr/local/share/mise/mise.toml "$HOME/.config/mise/config.toml"
fi

# Hand HOME to the remapped user. The full-tree walk runs only when a remap
# happened (first start on this container, or a changed workspace owner):
# usermod -u updates uids but leaves stale gids on skel files, and groupmod
# alone leaves the old gid behind. Idempotent restarts skip the walk.
chown "$uid:$gid" "$HOME"
if [ "$remapped" = "1" ]; then
    find "$HOME" -mindepth 1 \( ! -uid "$uid" -o ! -gid "$gid" \) -exec chown "$uid:$gid" {} + || true
fi

# User-level shims for the shared runtimes (non-fatal: the system shims stay
# on PATH as a fallback).
setpriv --reuid="$uid" --regid="$gid" --clear-groups mise reshim ||
    echo "mcpod: warning: mise reshim failed (system shims remain active)" >&2

# ---------------------------------------------------------------------------
# Drop privileges and exec (PID 1 becomes the mcpod server; SIGTERM/SIGINT
# from `docker stop` reach it directly).
# ---------------------------------------------------------------------------
echo "mcpod: workspace $workspace owned by $uid:$gid; running server as mcpod (sudo enabled)" >&2
exec setpriv \
    --reuid="$uid" \
    --regid="$gid" \
    --init-groups \
    "$mcpod_bin" "$@"
