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
  --security-opt no-new-privileges \
  fb0sh/mcpod:latest
```

镜像地址:[hub.docker.com/r/fb0sh/mcpod](https://hub.docker.com/r/fb0sh/mcpod)(标签:`latest`、`1.0.0`)

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
| `MCPOD_ALLOWED_HOSTS` | *(回环默认)* | `Host` 头允许列表(逗号分隔) |
| `MCPOD_ALLOWED_ORIGINS` | *(localhost 系默认)* | `Origin` 允许列表;默认放行 localhost/127.0.0.1/[::1] 任意端口 |
| `MCPOD_SSE_SESSION_TTL` | `30m` | 断开的 legacy SSE 会话保留时长 |
| `MCPOD_SSE_KEEPALIVE` | `15s` | SSE `: keepalive` 心跳间隔 |

**鉴权**:设置 `MCPOD_TOKEN` 后,`POST /mcp`、`GET /sse`、`POST /messages` 都要求 `Authorization: Bearer <token>`,失败返回 401;`GET /health` 永远免鉴权。**不设置则不鉴权**,任何能访问端口的人都能控制容器——启动日志会明确打印此警告。

## 工具语义

- **read** — `{"path", "offset"?, "limit"?}`:1 起始行号分页,头部截断于 2000 行 / 50 KiB,附 `Use offset=N to continue` 提示;图片(png/jpeg/gif/webp/bmp,按内容识别)以 MCP image block 返回,超过 2000px 等比缩小。
- **bash** — `{"command", "timeout"?}`:`/bin/bash -c`,工作目录为工作区;**无默认超时**。stdout/stderr 按到达顺序合并;尾部截断于 2000 行 / 50 KiB,完整输出存至 `/tmp/mcpod-bash-<id>.log`。超时与请求取消会杀掉整个进程树(进程组);非零退出保留输出并标记 error。
- **edit** — `{"path", "edits": [{"oldText", "newText"}]}`:全部 `oldText` 对**原始文件**匹配、必须唯一、不得重叠;整单原子。保留 CRLF 与 UTF-8 BOM;智能引号/破折号等 Unicode 归一化回退提升匹配鲁棒性;成功返回 `firstChangedLine + diff + patch`。
- **write** — `{"path", "content"}`:自动建父目录,原子替换。同一文件的并发写入经 per-file 队列串行。

所有文件工具接受相对路径或工作区内绝对路径,并限制在 `MCPOD_WORKSPACE` 内:`../` 穿越、`/workspace-evil` 前缀、symlink 逃逸均被 canonicalize 拒绝。`bash` 是容器级能力——Docker 边界(非 privileged、no-new-privileges、仅绑定 localhost)就是安全边界。

## 开发

```bash
cd mcp-server
cargo test     # 126 个测试:双 transport、协议、鉴权、工具、截断、并发
cargo clippy
```

服务器也可脱离 Docker 直接运行(`MCPOD_TOKEN=dev MCPOD_WORKSPACE=/tmp/ws cargo run`),集成测试即以此驱动真实 HTTP 面;另含官方 rmcp client 的黑盒测试与 2024-11-05 SSE 兼容测试。

```bash
scripts/acceptance.sh   # 容器验收:双 transport + 工具矩阵
```

## 目录结构

```
MCPod/
├── Dockerfile              # 多阶段:rust builder -> debian:13-slim
├── compose.yaml            # 仅 localhost, no-new-privileges
├── docs/structure.png      # 架构图
├── scripts/acceptance.sh   # 容器验收脚本
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
