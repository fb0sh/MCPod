# MCPod

**MCP 控制的开发容器** —— 把一个完整的 Linux 开发环境变成 AI Agent 可以可靠操作的 MCP Endpoint。

![架构图](docs/structure.png)

MCPod 不把每种开发能力都封装成 MCP Tool,只提供四个通用原语,其余交给完整 Linux 环境:

| MCP Tool | 用途 |
|----------|------|
| `read`   | 读取文件内容(文本分页 / 图片) |
| `bash`   | 执行 Shell 命令(编译、测试、git、mise、包管理器……) |
| `edit`   | 精确多处文本替换(带 diff) |
| `write`  | 创建或完全重写文件 |

工具语义复刻 Pi Coding Agent:read 头部截断(2000 行 / 50 KiB)+ `offset` 续读;bash 尾部截断并把完整输出落盘 `/tmp/mcpod-bash-*.log`;edit 的 `edits[]` 对原始文件匹配、全有或全无;所有写入经 per-file 队列 + 原子 rename 提交。

运行时(node/python/rust/go……)由容器内 [mise](https://mise.jdx.dev) 按项目自己的 `mise.toml` 管理。

## 双 Transport

MCPod 同时支持两套远程 MCP Transport:

| Transport | Endpoint | 状态 |
|-----------|----------|------|
| **Streamable HTTP**(2026-07-28 / 2025-11-25) | `POST /mcp` | **主推** |
| Legacy HTTP+SSE(2024-11-05) | `GET /sse` + `POST /messages` | 兼容旧客户端 |

`/sse` 仅为兼容只支持 SSE 的旧版 MCP Client;新客户端一律使用 `/mcp`。

```
AI Agent ── MCP ──> MCPod Server ──> Debian 13 容器
                    · 双 Transport   · mise, git, gcc, rg, ...
                    · Bearer 鉴权    · /workspace
                    · 4 个工具 + environment 资源
```

## 快速开始

### 方式一:docker compose(源码构建)

```bash
git clone <this-repo> && cd MCPod
export MCPOD_TOKEN="$(openssl rand -hex 32)"   # 不设则不鉴权(见下)
docker compose up -d                            # 仅绑定 127.0.0.1:3000

curl http://localhost:3000/health               # -> {"status":"ok"}
```

### 方式二:docker pull / run(Docker Hub)

```bash
docker run -d --name mcpod \
  -p 127.0.0.1:3000:3000 \
  -e MCPOD_TOKEN="$(openssl rand -hex 32)" \
  -v "$PWD/workspace:/workspace" \
  fb0sh/mcpod:latest
```

镜像地址:[hub.docker.com/r/fb0sh/mcpod](https://hub.docker.com/r/fb0sh/mcpod)(标签:`latest`、`2.0.0`)

### 方式三:主机直接运行(无 Docker)

从 [GitHub Releases](https://github.com/fb0sh/MCPod/releases) 下载预编译的 `mcpod` 二进制
(`mcpod-linux-x64.tar.gz`、`mcpod-macos-arm64.tar.gz`,附 `.sha256` 校验),解压即用:

```bash
tar -xzf mcpod-macos-arm64.tar.gz
MCPOD_TOKEN="$(openssl rand -hex 32)" MCPOD_WORKSPACE="$PWD" ./mcpod
# -> mcpod server started addr=127.0.0.1:3000 workspace=...
```

Linux 版为完全静态的 musl 二进制,可在任意 x86_64 发行版(含 Alpine/老旧 glibc)运行;macOS 版为 arm64。也可以从源码构建:`cd mcp-server && cargo build --release`。它与容器内运行的是同一个服务器,Agent 客户端配置完全相同。

注意:主机模式下 `read/write/edit/bash` 直接操作主机文件系统,bash 以当前用户身份执行,没有 Docker 隔离边界——请仅在可信环境使用,并务必设置 `MCPOD_TOKEN`(默认只绑定 `127.0.0.1`)。

### Agent 客户端配置

现代客户端(Streamable HTTP,推荐):

```json
{
  "mcpServers": {
    "mcpod": {
      "type": "http",
      "url": "http://localhost:3000/mcp",
      "headers": {
        "Authorization": "Bearer ${MCPOD_TOKEN}"
      }
    }
  }
}
```

旧版 SSE 客户端(兼容模式):

```text
URL:  http://localhost:3000/sse
认证: Bearer ${MCPOD_TOKEN}
```

两种 Transport 的 tools / resources / instructions 完全一致(同一 MCPod 核心,双适配器)。

## 配置

| 环境变量 | 默认 | 说明 |
|----------|------|------|
| `MCPOD_TOKEN` | *(未设置)* | Bearer Token。**未设置时关闭鉴权**(启动日志会打警告) |
| `MCPOD_PORT` | `3000` | 监听端口 |
| `MCPOD_WORKSPACE` | `/workspace` | 工作区根目录,文件工具被限制在内 |
| `MCPOD_HOST` | `127.0.0.1`(镜像内 `0.0.0.0`) | 绑定地址 |
| `MCPOD_ALLOWED_HOSTS` | *(不限制)* | `Host` 头白名单(逗号分隔)。默认不限制:IP/域名/反代均可访问;设置后仅放行列表内 Host |
| `MCPOD_ALLOWED_ORIGINS` | *(localhost 系默认)* | `Origin` 允许列表;默认放行 localhost/127.0.0.1/[::1] 任意端口 |
| `MCPOD_SSE_SESSION_TTL` | `30m` | 断开的 legacy SSE 会话保留时长 |
| `MCPOD_SSE_KEEPALIVE` | `15s` | SSE `: keepalive` 心跳间隔 |

**鉴权**:设置 `MCPOD_TOKEN` 后,`POST /mcp`、`GET /sse`、`POST /messages` 都要求 `Authorization: Bearer <token>`,失败返回 401;`GET /health` 永远免鉴权。**不设置则不鉴权**,任何能访问端口的人都能控制容器——启动日志会明确打印此警告。

## 容器权限模型

MCPod 容器启动时由 entrypoint(`scripts/docker-entrypoint.sh`)自动完成身份映射,**零配置**:

```text
stat $MCPOD_WORKSPACE 的属主 UID/GID
        ↓
usermod/groupmod 把镜像内 mcpod 用户重映射到该 UID/GID
        ↓
准备可写的 /home/mcpod($HOME:mise 用户级状态、.gitconfig、.ssh……)
        ↓
setpriv 降权,整个 MCPod 进程树以 mcpod 身份运行
```

因此:

- **文件 ownership 正确**:`read`/`write`/`edit`/`bash` 产生的文件(包括 `edit` 的原子 rename)都归属宿主 workspace 用户。Linux bind mount 下不会再出现 `root:root` 文件。
- **无需任何配置**:不用设置 `PUID`/`PGID`/`UID`/`GID` 环境变量,不用 `--user`,不用 `chown -R`。`docker compose up -d` 或 `docker run -v "$PWD/workspace:/workspace" ...` 即可。
- **Agent 有 passwordless sudo**:Agent 在容器内是普通用户 `mcpod`,但可以 `sudo apt-get install -y <pkg>` 安装系统软件,装完立即可用,无需重启。
- **sudo 的 ownership 语义**:显式 `sudo touch /workspace/foo` 产生 root 属主文件——这是标准 Linux 行为,普通操作请不要加 sudo。
- **HOME 稳定可写**:`$HOME=/home/mcpod`,git/ssh/pip/mise 等的用户级配置与缓存都落在这里;mise 预装 runtime(`/usr/local/share/mise`)只读共享,Agent 自己 `mise install` 的新 runtime 装入 `$HOME`。
- **UID/GID 冲突安全**:目标 UID/GID 与镜像内已有用户/组冲突时用 `usermod/groupmod -o` 处理,`sudo`、`getpwuid()`、git 等均正常。

两个特殊场景:

- **root 属主的 workspace**(如 Docker Desktop 的文件共享挂载、root 属主 volume):MCPod 保持以 root 运行(即旧行为),Docker Desktop 下宿主文件属主由其文件共享层控制,macOS/Windows 上不必依赖此 UID 映射。
- **自定义 `MCPOD_WORKSPACE`**:`docker run -e MCPOD_WORKSPACE=/project -v "$PWD:/project" ...` 同样生效,entrypoint 读取的是 `$MCPOD_WORKSPACE` 的属主。

### 安全边界

MCP 的 `bash` 本身就是任意命令执行能力;加上 passwordless sudo 后,**拿到 MCPod endpoint 控制权 ≈ 拿到该容器 root 控制权**(但仅限容器自身与用户显式挂载进容器的目录)。请务必:

- 设置 `MCPOD_TOKEN`,保持默认 `127.0.0.1` 端口绑定,不要把未认证的 endpoint 暴露到不可信网络;
- 不挂载 Docker socket(`/var/run/docker.sock`)、不使用 `--privileged`、不挂载 Agent 确实需要访问之外的宿主目录;
- 需要更严格的隔离时,可自行加回 `security_opt: [no-new-privileges:true]`——代价是 Agent 失去 sudo 提权能力。

## 工具语义

- **read** — `{"path", "offset"?, "limit"?}`:1 起始行号分页,头部截断于 2000 行 / 50 KiB,附 `Use offset=N to continue` 提示;图片(png/jpeg/gif/webp/bmp,按内容识别)以 MCP image block 返回,超过 2000px 等比缩小。
- **bash** — `{"command", "timeout"?}`:`/bin/bash -c`,工作目录为工作区;**无默认超时**。stdout/stderr 按到达顺序合并;尾部截断于 2000 行 / 50 KiB,完整输出存至 `/tmp/mcpod-bash-<id>.log`。超时与请求取消会杀掉整个进程树(进程组);非零退出保留输出并标记 error。
- **edit** — `{"path", "edits": [{"oldText", "newText"}]}`:全部 `oldText` 对**原始文件**匹配、必须唯一、不得重叠;整单原子。保留 CRLF 与 UTF-8 BOM;智能引号/破折号等 Unicode 归一化回退提升匹配鲁棒性;成功返回 `firstChangedLine + diff + patch`。
- **write** — `{"path", "content"}`:自动建父目录,原子替换。同一文件的并发写入经 per-file 队列串行。

所有文件工具接受相对路径或工作区内绝对路径,并限制在 `MCPOD_WORKSPACE` 内:`../` 穿越、`/workspace-evil` 前缀、symlink 逃逸均被 canonicalize 拒绝。`bash` 是容器级能力——Agent 以 `mcpod` 用户运行并可通过 sudo 成为容器 root,Docker 边界(非 privileged、不挂 Docker socket、仅绑定 localhost)就是安全边界,详见「容器权限模型」。

## 开发

```bash
cd mcp-server
cargo test     # 127 个测试:双 transport、协议、鉴权、工具、截断、并发
cargo clippy
```

服务器也可脱离 Docker 直接运行(`MCPOD_TOKEN=dev MCPOD_WORKSPACE=/tmp/ws cargo run`),集成测试即以此驱动真实 HTTP 面;另含官方 rmcp client 的黑盒测试与 2024-11-05 SSE 兼容测试。

```bash
scripts/acceptance.sh   # 容器验收:双 transport + 工具矩阵
```

## 目录结构

```
MCPod/
├── .github/workflows/     # CI:测试门禁 + 发布 mcpod 主机二进制(linux-x64 / macos-arm64)
├── Dockerfile              # 多阶段:rust builder -> debian:13-slim(mcpod 用户 + sudo)
├── docker-entrypoint.sh    # scripts/:workspace 属主映射 + 降权(见「容器权限模型」)
├── compose.yaml            # 仅 localhost 绑定
├── docs/structure.png      # 架构图
├── scripts/acceptance.sh   # 容器验收脚本(含 ownership 回归)
├── mcp-server/             # Rust MCP 服务器(rmcp + axum + tokio)
│   └── src/
│       ├── transport/      # streamable_http(/mcp)+ legacy_sse(/sse + /messages)
│       ├── tools/          # read, bash, edit, write
│       ├── fs/             # 路径沙箱、mutation 队列、原子写、文本处理
│       ├── output/         # 头/尾截断(2000 行 / 50 KiB)
│       └── process/        # 进程组执行与整树击杀
└── workspace/              # 挂载到容器 /workspace
```

## 许可

MIT
