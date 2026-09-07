# AGENTS.md — project rules for agents working in this workspace

This file is read by AI agents before they modify the project. Replace its
content with rules that fit your project; this is only a starting point.

## Workspace

- You are working inside an MCPod development container.
- The workspace root is `/workspace`; keep all project files inside it.
- Only mounted directories persist across container recreation.

## Runtimes

- Runtime versions (node, python, rust, go, ...) are managed by `mise`.
- If `mise.toml` exists, run `mise install` before building or testing.
- Prefer `mise exec -- <command>` (or the installed shims) over system binaries.

## Conventions

- Keep commits small and focused.
- Run the test suite before declaring work done.
- Never commit secrets or generated credentials.
