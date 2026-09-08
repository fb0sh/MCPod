MCPod PRD

Project: MCPod
Version: v0.1
定位： MCP-controlled Development Container
状态： MVP

1. 产品概述
1.1 产品名称

MCPod

1.2 一句话定义

MCPod 是一个由 MCP 控制的隔离式 Docker 开发环境，为 AI Agent 提供标准化的 Linux 开发工作区。

1.3 核心理念

MCPod 不试图把每一种开发能力都封装成 MCP Tool。

它只提供最基础的操作能力：

bash
read
write
edit

复杂能力全部交给容器本身：

git
gcc
clang
node
python
rust
go
npm
pnpm
cargo
make
...

运行时和语言版本由：

mise

负责管理。

因此：

AI Agent
    │
    │ MCP
    ▼
 MCPod MCP Server
    │
    ▼
 Docker Container
    │
    ├── Linux
    ├── mise
    ├── development tools
    └── /workspace
2. 产品目标
2.1 MVP 必须实现
MCP

支持 MCP 标准：

initialize
tools/list
tools/call
resources/list
resources/read
Streamable HTTP Transport
最新 MCP 协议版本兼容
Stateless MCP Server
Tools

提供：

bash
read
write
edit
Instructions

MCP Server 初始化时返回：

instructions

告诉 Agent：

当前环境
工作目录
Tool 使用方式
mise 使用方式
挂载目录
AGENTS.md 规则
Resource

提供：

resource://environment

返回动态环境信息。

Container

默认：

Debian 13

包含：

bash
git
curl
wget
ca-certificates
build-essential
pkg-config
jq
file
less
unzip
zip
openssh-client
procps
psmisc
sudo
mise
Authentication

MCP HTTP Endpoint 使用：

Authorization: Bearer <token>

进行简单鉴权。

Workspace

默认工作目录：

/workspace

MCP 文件操作默认限制在：

/workspace
3. 非目标

MVP 不实现：

OAuth
多用户系统
Web UI
Kubernetes
容器集群
容器自动扩缩
MCP Tool marketplace
复杂权限系统
Docker-in-Docker
文件同步服务
IDE
LSP Server
Git 专用 MCP Tool
npm 专用 MCP Tool
Python 专用 MCP Tool

原则：

MCPod 是一个开发环境，不是一个 MCP 工具集合。

4. 核心架构
                    AI Agent
                       │
                       │ MCP
                       │ Streamable HTTP
                       ▼
              ┌──────────────────┐
              │   MCPod Server   │
              │                  │
              │ Authentication   │
              │ MCP Protocol     │
              │ Tool Handler     │
              │ Resource Handler │
              └────────┬─────────┘
                       │
                       ▼
              ┌──────────────────┐
              │ Docker Container │
              │                  │
              │ Debian 13        │
              │ mise             │
              │ git              │
              │ compilers        │
              │ development tools│
              │                  │
              │ /workspace       │
              └──────────────────┘
5. 技术栈
MCP Server

使用 Rust。

推荐：

Rust
Tokio
rmcp
Axum
Serde
Serde JSON

原则：

不自行实现 MCP 协议。

优先使用官方/主流 Rust MCP SDK 提供的实现。

6. Docker 镜像
Base Image

使用：

debian:13-slim

不使用 Alpine。

原因：

glibc 兼容性更好
Node/Python/Rust 生态兼容性好
编译环境更少出现 musl 问题
适合通用开发环境
7. Dockerfile

最终镜像结构：

Debian 13
│
├── bash
├── git
├── curl
├── wget
├── jq
├── build-essential
├── pkg-config
├── openssh-client
├── ...
│
├── mise
│
├── /usr/local/bin/mcpod
│
└── /workspace

MCP Server 使用 multi-stage build：

Rust Builder
    │
    │ cargo build --release
    ▼
Debian Runtime
    │
    └── /usr/local/bin/mcpod

最终镜像不应该包含完整 Rust 编译环境，除非未来提供 Rust 预装 profile。

8. mise

MCPod 默认内置：

mise

Mise 用于管理开发环境 Runtime。

例如项目：

[tools]
node = "24"
python = "3.13"
rust = "stable"

Agent 可以通过：

mise install

安装项目环境。

MCPod 不在基础镜像中固定安装：

node
python
rust
go
java

这些由项目自己的 mise.toml 决定。

9. MCP Endpoint

默认：

POST /mcp

例如：

http://localhost:3000/mcp

MCP 使用：

Streamable HTTP

不实现旧式：

/sse
/messages

架构必须按照当前 MCP Streamable HTTP 规范实现。

10. Authentication

使用 Bearer Token。

环境变量：

MCPOD_TOKEN

例如：

MCPOD_TOKEN=change-me

请求：

Authorization: Bearer change-me

认证失败：

401 Unauthorized

缺少 Token：

401 Unauthorized

Token 错误：

401 Unauthorized
11. Health Check

提供：

GET /health

不需要 MCP Authentication。

成功：

{
  "status": "ok"
}

用于 Docker Healthcheck。

12. MCP Instructions

MCP Server 的 initialize 响应必须包含 instructions。

示例：

You are connected to an MCPod development container.

Workspace:
- The project workspace is /workspace.
- Perform project work inside /workspace.

Available tools:
- read: Read files.
- write: Create or replace files.
- edit: Modify existing files.
- bash: Execute shell commands.

Runtime:
- mise is installed.
- Use mise to manage development runtimes.
- Check mise.toml when present.

Project instructions:
- If /workspace/AGENTS.md exists, read it before modifying the project.

Persistence:
- Only mounted directories are guaranteed to persist.
- Do not assume changes outside mounted directories survive container recreation.

Instructions 的目标：

让 Agent 在连接 MCP 后立即知道如何使用 MCPod。

13. MCP Tools
13.1 bash
Purpose

执行容器内 Shell 命令。

Input
{
  "command": "string"
}
Example
bash("git status")
bash("mise install")
bash("cargo test")
bash("npm run build")
Working Directory

默认：

/workspace
注意

不要人为禁止：

rm
sudo
chmod
chown

等命令。

Docker Container 本身就是安全边界。

MCP Server 不应该试图通过字符串黑名单构造一个“伪沙箱”。

14. read
Purpose

读取文件。

Input
{
  "path": "src/main.rs",
  "start_line": 1,
  "end_line": 100
}

start_line 和 end_line 可选。

默认

如果没有指定范围：

读取整个文件

但对于超大文件应该考虑限制返回大小。

15. write
Purpose

创建或覆盖文件。

Input
{
  "path": "src/config.rs",
  "content": "..."
}
16. edit
Purpose

修改已有文件。

Input
{
  "path": "src/main.rs",
  "old": "old content",
  "new": "new content"
}
行为

如果：

old

在文件中不存在：

返回错误

如果匹配多个位置：

默认返回错误

避免 Agent 错误修改多个位置。

可以考虑未来增加：

replace_all

但 MVP 不需要。

17. Workspace Security

文件 Tool：

read
write
edit

必须限制在：

/workspace

禁止：

../../etc/passwd

或者：

/etc/passwd

等路径逃逸。

路径处理必须：

resolve
canonicalize
检查最终路径是否位于 /workspace

不能简单：

path.starts_with("/workspace")

因为：

/workspace-evil

也会通过。

18. Bash Security

Bash 与文件 Tool 不同。

Bash 是：

container-level capability

因此：

bash("cat /etc/passwd")

理论上允许。

这是设计行为。

安全边界：

Host
 ↓
Docker
 ↓
Container
 ↓
MCP

而不是：

MCP
 ↓
危险命令黑名单
19. Environment Resource

提供：

resource://environment

内容动态生成。

例如：

{
  "os": "Debian GNU/Linux 13",
  "architecture": "x86_64",
  "workspace": "/workspace",
  "mounts": [
    {
      "path": "/workspace",
      "mode": "rw",
      "persistent": true
    }
  ],
  "runtime_manager": {
    "name": "mise",
    "installed": true
  }
}
20. Mount 信息

MCP Server 不应该把 Docker Compose 写死。

应该动态获取当前 Container 的实际 Mount 信息。

例如：

/workspace     rw
/data          rw
/config        ro

返回：

{
  "path": "/workspace",
  "mode": "rw",
  "persistent": true
}

敏感信息不要暴露。

21. AGENTS.md

MCPod 不提供：

agents_md()

Tool。

因为：

AGENTS.md

本身就是项目文件。

Agent 应该：

read("/workspace/AGENTS.md")

获取项目规则。

MCP instructions 只负责告诉 Agent：

If /workspace/AGENTS.md exists, read it before modifying the project.

形成：

MCP Instructions
       │
       ▼
Container-level instructions
       │
       ▼
AGENTS.md
       │
       ▼
Project-level instructions
22. Container Environment

默认环境：

WORKDIR=/workspace

MCP Server：

/usr/local/bin/mcpod

默认端口：

3000

环境变量：

MCPOD_TOKEN
MCPOD_PORT
MCPOD_WORKSPACE

其中：

MCPOD_WORKSPACE

默认：

/workspace
23. Docker Compose

提供：

compose.yaml

最小配置：

services:
  mcpod:
    build: .
    ports:
      - "127.0.0.1:3000:3000"
    environment:
      MCPOD_TOKEN: ${MCPOD_TOKEN}
    volumes:
      - ./workspace:/workspace
    restart: unless-stopped

原则：

默认只绑定 localhost。

不要默认：

0.0.0.0:3000

暴露 MCP。

24. 项目目录结构

建议：

mcpod/
├── Dockerfile
├── compose.yaml
├── README.md
├── LICENSE
├── .dockerignore
│
├── mcp-server/
│   ├── Cargo.toml
│   ├── Cargo.lock
│   └── src/
│       ├── main.rs
│       ├── server.rs
│       ├── auth.rs
│       ├── instructions.rs
│       ├── environment.rs
│       └── tools/
│           ├── mod.rs
│           ├── bash.rs
│           ├── read.rs
│           ├── write.rs
│           └── edit.rs
│
└── workspace/
    └── AGENTS.md
25. MCP Server 启动流程
Container Start
       │
       ▼
mcpod
       │
       ├── Load configuration
       │
       ├── Validate MCPOD_TOKEN
       │
       ├── Initialize MCP Server
       │
       ├── Register Tools
       │
       ├── Register Resources
       │
       └── Start HTTP Server
                    │
                    ▼
               :3000/mcp
26. MCP Client 工作流程

理想情况下：

Agent
  │
  │ POST /mcp
  ▼
initialize
  │
  ▼
instructions
  │
  ▼
tools/list
  │
  ├── bash
  ├── read
  ├── write
  └── edit

如果 Agent 支持并使用 Resources：

resources/list
      ↓
resource://environment
      ↓
resources/read

但 MCPod 不能依赖 Client 一定读取 Resource。

核心使用说明必须存在于：

initialize.instructions
27. Tool Description 原则

Tool description 必须足够详细，让 LLM 知道：

bash

什么时候使用：

Use bash for:
- shell commands
- compilation
- tests
- git
- package management
- searching
- mise
read
Use read to inspect file contents.
Prefer this over `cat` when reading source files.
edit
Use edit to modify existing files.
Prefer this over shell-based text replacement.
write
Use write to create new files or completely replace file contents.

这样即使 Agent 没有读取 Resource，也能理解 MCPod。

28. Error Handling

所有 Tool 都必须返回结构化、对 Agent 有帮助的错误。

例如：

File not found:
src/main.rs

而不是：

error

edit 失败：

The provided old content was not found in src/main.rs.

多次匹配：

The provided old content matched 4 locations.
Refusing to modify the file.

bash：

exit_code: 1
stdout: ...
stderr: ...
29. Bash 返回格式

建议：

{
  "exit_code": 0,
  "stdout": "...",
  "stderr": ""
}

如果命令失败：

{
  "exit_code": 1,
  "stdout": "...",
  "stderr": "..."
}

不要只返回字符串。

这样 Agent 更容易判断：

成功
失败
警告
30. 日志

MCP Server 日志输出：

stderr

不要污染：

stdout

因为 MCP/HTTP 服务本身可能需要标准输出用于其他用途。

日志至少包括：

server started
request received
tool called
tool completed
authentication failed

但：

禁止记录 Bearer Token。

31. 配置

MVP 使用环境变量：

MCPOD_PORT=3000
MCPOD_TOKEN=...
MCPOD_WORKSPACE=/workspace

未来可以增加：

mcpod.toml

但 MVP 不需要配置文件系统。

32. 默认 Container 用户

当前实现（v1.1+，scripts/docker-entrypoint.sh）：

镜像内创建 mcpod 用户（passwordless sudo）。
容器启动时 entrypoint 读取 MCPOD_WORKSPACE 属主 UID/GID，
把 mcpod 重映射到该身份后降权运行 MCP Server，
使 bind mount 中产生的文件保持宿主属主。

Agent 在容器内是普通用户，可通过 sudo 成为容器 root
（安装系统软件等）；root 属主 workspace（如 Docker Desktop
文件共享）回退为 root 运行。

注意：因需要支持 sudo 提权，compose 不再设置
no-new-privileges；安全边界仍是非 privileged、
不挂 Docker socket、仅绑定 localhost（见 README）。

仍应避免：

privileged

33. 网络

默认情况下：

Container
   │
   └── outbound network

允许开发环境正常访问：

npm registry
crates.io
PyPI
GitHub

MVP 不做复杂网络策略。

34. 镜像设计

第一阶段只有：

mcpod:latest

未来可以：

mcpod:base
mcpod:rust
mcpod:python
mcpod:node
mcpod:security

但这些不是 MVP。

核心原则：

MCP API 与镜像内容解耦。

无论容器里有没有 Rust：

bash
read
write
edit

都保持不变。

35. 可扩展性

未来可以增加 MCP Tools：

terminal
process

但必须谨慎。

不要为了“功能丰富”变成：

100 个 MCP Tools

MCPod 的核心优势就是：

少量通用 Primitive + 完整 Linux 环境。

36. 测试要求
MCP Protocol

测试：

initialize
tools/list
tools/call
resources/list
resources/read
Streamable HTTP
invalid request
unsupported method
malformed JSON-RPC
Authentication

测试：

无 Authorization → 401
错误 Token → 401
正确 Token → 允许
Files

测试：

read
write
edit

以及：

../
../../
/etc/passwd
/workspace-evil

路径逃逸。

Bash

测试：

pwd
git --version
mise --version

以及：

exit code
stdout
stderr
37. Acceptance Criteria

MVP 完成的标准：

Docker
docker compose up -d

能够成功启动。

Health
curl http://localhost:3000/health

返回：

{"status":"ok"}
MCP

Agent 可以通过：

http://localhost:3000/mcp

完成 MCP 初始化。

Instructions

初始化结果包含 MCPod 使用说明。

Tools

Agent 可以：

bash
read
write
edit
Resource

Server 暴露：

resource://environment
mise

容器中：

mise --version

正常。

Workspace
/workspace

可以读写。

Authentication

没有正确 Token：

401
Security

MCP Server 默认：

127.0.0.1:3000

Docker 不使用：

privileged
38. MVP 用户体验

最终用户只需要：

git clone MCPod
cd MCPod

export MCPOD_TOKEN="$(openssl rand -hex 32)"

docker compose up -d

然后在 Agent 中配置：

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

然后：

Agent
  ↓
连接 MCPod
  ↓
获得 instructions
  ↓
发现 bash/read/write/edit
  ↓
开始开发
  ↓
mise install
  ↓
编译 / 测试 / 修改代码
39. 产品核心原则

整个项目开发过程中必须遵守这 6 条原则：

① MCP 是接口，不是开发环境
MCP → 控制
Docker → 环境
mise → Runtime
② Bash 是万能 Primitive

不要为每一个开发动作创建 MCP Tool。

③ Read/Edit/Write 是 Agent 优化

减少模型 Context 和错误率。

④ Container 是安全边界

不要用 MCP Tool 黑名单假装安全。

⑤ Instructions 是必需的

不能依赖 Agent 自动读取 Resource。

⑥ 保持 MCP API 极简
bash
read
write
edit

就是 MCPod 的核心。

40. 最终产品形态
                       ┌──────────────┐
                       │  AI Agent    │
                       └──────┬───────┘
                              │
                         Streamable HTTP
                              │
                              ▼
                    ┌──────────────────┐
                    │      MCPod       │
                    │                  │
                    │  instructions    │
                    │  bash            │
                    │  read            │
                    │  write           │
                    │  edit            │
                    │  environment     │
                    └────────┬─────────┘
                             │
                             ▼
                    ┌──────────────────┐
                    │ Docker Container │
                    │                  │
                    │ Debian 13        │
                    │ mise             │
                    │ git              │
                    │ gcc               │
                    │ curl              │
                    │ jq                │
                    │ ...              │
                    │                  │
                    │ /workspace        │
                    └────────┬─────────┘
                             │
                             ▼
                         Project

MCPod 的核心不是“拥有很多 MCP 工具”，而是把一个完整的 Linux 开发环境变成 AI Agent 可以可靠操作的 MCP Endpoint。

这应该就是 v0.1 的边界。后续如果要继续做，我会优先增加容器生命周期管理、镜像 Profile、持久化 Workspace 和非 root 模式，而不是增加更多 MCP Tool。
