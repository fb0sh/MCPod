#!/usr/bin/env bash
# MCPod acceptance run (docs/plan.md §37 + pi-spec tool semantics).
# Requires a running container at 127.0.0.1:3000 and MCPOD_TOKEN exported.
# Sections 14-16 verify the permission model (scripts/docker-entrypoint.sh):
# the mcpod user mapped onto the workspace owner, passwordless sudo, and
# file-ownership preservation for write/edit/bash. Never assumes uid 1000.
# Optional network round-trips: MCPOD_ACCEPTANCE_APT=1 (apt install),
# MCPOD_ACCEPTANCE_MISE=1 (user-level mise install).
set -euo pipefail

BASE="${MCPOD_BASE:-http://127.0.0.1:3000}"
AUTH="Authorization: Bearer ${MCPOD_TOKEN:?export MCPOD_TOKEN first}"
JSON="Content-Type: application/json"
ACCEPT="Accept: application/json, text/event-stream"

pass=0; fail=0
check() { # name, condition-result
  if [ "$2" = "0" ]; then pass=$((pass+1)); echo "PASS: $1"; else fail=$((fail+1)); echo "FAIL: $1"; fi
}

post() { curl -s -X POST "$BASE/mcp" -H "$AUTH" -H "$JSON" -H "$ACCEPT" --data "$1"; }

# 1. Health (§11)
[ "$(curl -s "$BASE/health")" = '{"status":"ok"}' ]
check "health returns {\"status\":\"ok\"}" $?

# 2. Auth (§10): no token and wrong token are both 401
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/mcp" -H "$JSON" -H "$ACCEPT" --data '{}')
[ "$code" = "401" ]; check "missing token -> 401" $?
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/mcp" -H "Authorization: Bearer wrong" -H "$JSON" -H "$ACCEPT" --data '{}')
[ "$code" = "401" ]; check "wrong token -> 401" $?

# 3. initialize carries instructions (§12)
init=$(post '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"acceptance","version":"1.0"}}}')
echo "$init" | grep -q '"instructions"'
check "initialize returns instructions" $?
echo "$init" | grep -q 'MCPod development container'
check "instructions mention MCPod container" $?

# 4. tools/list -> read/bash/edit/write in pi-spec order
tools=$(post '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}')
names=$(echo "$tools" | jq -r '[.result.tools[].name] | join(",")')
[ "$names" = "read,bash,edit,write" ]
check "tools are read,bash,edit,write in order" $?
echo "$tools" | jq -e '.result.tools[] | select(.name=="edit") | .inputSchema.properties.edits' >/dev/null
check "edit schema exposes edits[]" $?
echo "$tools" | jq -e '.result.tools[] | select(.name=="read") | .inputSchema.properties.offset' >/dev/null
check "read schema exposes offset" $?

# 5. write + read roundtrip
post '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"write","arguments":{"path":"acceptance.txt","content":"mcpod-acceptance"}}}' >/dev/null
read_back=$(post '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"read","arguments":{"path":"acceptance.txt"}}}')
echo "$read_back" | grep -q 'mcpod-acceptance'
check "write + read roundtrip" $?

# 6. edit with edits[] (pi-spec §13)
edit_result=$(post '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"edit","arguments":{"path":"acceptance.txt","edits":[{"oldText":"mcpod","newText":"mcpod-v2"},{"oldText":"acceptance","newText":"edited"}]}}}')
echo "$edit_result" | jq -e '.result.isError == false' >/dev/null
check "edit applies multiple replacements" $?
echo "$edit_result" | jq -e '.result.structuredContent.replacements == 2' >/dev/null
check "edit reports replacements + diff" $?

# 7. bash: pwd, mise, exit codes, stderr, no output
bash_out=$(post '{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"bash","arguments":{"command":"pwd"}}}')
[ "$(echo "$bash_out" | jq -r '.result.structuredContent.stdout' | tr -d '\n')" = "/workspace" ]
check "bash pwd -> /workspace" $?
bash_out=$(post '{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"bash","arguments":{"command":"mise --version"}}}')
[ "$(echo "$bash_out" | jq -r '.result.structuredContent.exit_code')" = "0" ]
check "bash mise --version exit 0" $?
bash_out=$(post '{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"bash","arguments":{"command":"echo out; echo err >&2; exit 3"}}}')
[ "$(echo "$bash_out" | jq -r '.result.structuredContent.exit_code')" = "3" ]
check "bash propagates exit code 3" $?
echo "$bash_out" | jq -e '.result.structuredContent.stderr | test("err")' >/dev/null
check "bash captures stderr" $?
echo "$bash_out" | grep -q 'Command exited with code 3'
check "bash failure message included" $?
bash_out=$(post '{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"bash","arguments":{"command":"true"}}}')
[ "$(echo "$bash_out" | jq -r '.result.content[0].text')" = "(no output)" ]
check "bash empty output -> (no output)" $?

# 8. bash tail truncation saves full output to /tmp log (pi-spec §30-31)
bash_out=$(post '{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"bash","arguments":{"command":"seq 1 30000","timeout":120}}}')
echo "$bash_out" | grep -q '\[Showing lines 28001-30000 of 30000. Full output: /tmp/mcpod-bash-'
check "bash tail truncation with log path" $?
log=$(echo "$bash_out" | jq -r '.result.content[0].text' | grep -o '/tmp/mcpod-bash-[^]]*\.log' | head -1)
# The log lives inside the container; verify it there. Works with both
# `docker compose up` (service: mcpod) and a plain `docker run` container.
MCPod_CONTAINER="${MCPOD_CONTAINER:-$(docker compose ps -q mcpod 2>/dev/null || true)}"
if [ -z "$MCPod_CONTAINER" ]; then
  MCPod_CONTAINER=$(docker ps --filter name=mcpod --format '{{.ID}}' | head -1)
fi
if [ -z "$MCPod_CONTAINER" ]; then
  MCPod_CONTAINER=$(docker ps --filter ancestor=fb0sh/mcpod --format '{{.ID}}' | head -1)
fi
log_lines=$(docker exec "$MCPod_CONTAINER" sh -c "wc -l < '$log'" | tr -d ' \r')
[ "$log_lines" = "30000" ]
check "bash full output log contains all lines" $?
# Agent follow-up pattern (§31): grep the saved log via bash.
payload=$(jq -nc --arg cmd "rg -n '^29999$' $log" '{jsonrpc:"2.0",id:17,method:"tools/call",params:{name:"bash",arguments:{command:$cmd}}}')
bash_out=$(post "$payload")
echo "$bash_out" | grep -q '29999'
check "agent can grep the full output log" $?


# 12. Legacy SSE transport (transport-spec §3, §10-§12): full flow
sse_log=$(mktemp)
curl -s -N --max-time 20 "$BASE/sse" -H "$AUTH" > "$sse_log" &
sse_pid=$!
for _ in $(seq 1 50); do grep -q "event: endpoint" "$sse_log" 2>/dev/null && break; sleep 0.1; done
grep -q "event: endpoint" "$sse_log"
check "legacy SSE sends endpoint event" $?
SESSION_ID=$(grep -o 'sessionId=[a-f0-9-]*' "$sse_log" | head -1 | cut -d= -f2)
[ -n "$SESSION_ID" ]
check "endpoint event carries sessionId" $?

post_legacy() { curl -s -X POST "$BASE/messages?sessionId=$SESSION_ID" -H "$AUTH" -H "$JSON" --data "$1"; }
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/messages?sessionId=$SESSION_ID" -H "$AUTH" -H "$JSON" --data '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"acceptance","version":"1.0"}}}')
[ "$code" = "202" ]
check "legacy initialize accepted (202)" $?
post_legacy '{"jsonrpc":"2.0","method":"notifications/initialized"}' >/dev/null
post_legacy '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' >/dev/null
for _ in $(seq 1 50); do grep -q '"id":2' "$sse_log" 2>/dev/null && break; sleep 0.1; done
grep -q '"id":2' "$sse_log"
check "legacy tools/list result on SSE stream" $?
sse_names=$(grep '"id":2' "$sse_log" | grep -o '"name":"[a-z]*"' | head -4 | tr '
' ' ')
echo "$sse_names" | grep -q '"name":"read" "name":"bash"'
check "legacy tools order matches streamable HTTP" $?
post_legacy '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"bash","arguments":{"command":"echo legacy-ok","timeout":30}}}' >/dev/null
for _ in $(seq 1 50); do grep -q '"id":3' "$sse_log" 2>/dev/null && break; sleep 0.1; done
grep '"id":3' "$sse_log" | grep -q 'legacy-ok'
check "legacy tools/call bash works" $?
kill $sse_pid 2>/dev/null
rm -f "$sse_log"

# 13. Legacy SSE auth + session rules (§17, §45)
code=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/sse" --max-time 2)
[ "$code" = "401" ]; check "unauthorized /sse -> 401" $?
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/messages?sessionId=unknown" -H "$JSON" --data '{"jsonrpc":"2.0","id":9,"method":"ping"}')
[ "$code" = "401" ]; check "unauthorized /messages -> 401" $?
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/messages?sessionId=00000000-0000-4000-8000-000000000000" -H "$AUTH" -H "$JSON" --data '{"jsonrpc":"2.0","id":9,"method":"ping"}')
[ "$code" = "404" ]; check "unknown session -> 404" $?

# 14. Permission model (scripts/docker-entrypoint.sh): the mcpod user is
# remapped onto the workspace owner's uid/gid. Never assume 1000 — read the
# real owner from the mounted workspace inside the container.
ws_path="$(docker exec "$MCPod_CONTAINER" sh -c 'printf "%s" "${MCPOD_WORKSPACE:-/workspace}"' | tr -d '\r')"
ws_owner="$(docker exec "$MCPod_CONTAINER" stat -c '%u:%g' "$ws_path" | tr -d '\r')"
ws_uid="${ws_owner%%:*}"
ws_gid="${ws_owner##*:}"
[ -n "$ws_uid" ] && [ -n "$ws_gid" ]
check "workspace owner resolved: $ws_path -> $ws_owner" $?

mcp_bash() { # command -> raw JSON-RPC result of the bash tool
  post "$(jq -nc --arg cmd "$1" '{jsonrpc:"2.0",id:99,method:"tools/call",params:{name:"bash",arguments:{command:$cmd}}}')"
}
bash_out()  { mcp_bash "$1" | jq -r '.result.structuredContent.stdout' | tr -d '\r'; }
bash_code() { mcp_bash "$1" | jq -r '.result.structuredContent.exit_code' | tr -d '\r'; }

id_lines="$(bash_out 'id -u; id -g; whoami; echo $HOME')"
mcpod_uid="$(echo "$id_lines" | sed -n 1p)"
mcpod_gid="$(echo "$id_lines" | sed -n 2p)"
mcpod_user="$(echo "$id_lines" | sed -n 3p)"
mcpod_home="$(echo "$id_lines" | sed -n 4p)"
[ "$mcpod_uid" = "$ws_uid" ] && [ "$mcpod_gid" = "$ws_gid" ]
check "bash identity uid/gid == workspace owner ($ws_uid:$ws_gid)" $?

if [ "$ws_uid" != "0" ]; then
  [ "$mcpod_user" = "mcpod" ]; check "bash whoami -> mcpod" $?
  [ "$mcpod_home" = "/home/mcpod" ]; check "bash HOME -> /home/mcpod" $?
  [ "$(bash_out 'sudo -n whoami')" = "root" ]; check "sudo -n whoami -> root" $?
  [ "$(bash_code 'sudo -n true')" = "0" ]; check "sudo -n true exits 0 (no password)" $?
else
  # Root-owned workspace (e.g. Docker Desktop bind mounts): documented
  # fallback — the server keeps running as root.
  [ "$mcpod_user" = "root" ]; check "root-owned workspace: whoami -> root (fallback)" $?
  [ "$(bash_code 'sudo -n true')" = "0" ]; check "sudo -n true exits 0 (as root)" $?
fi

# The MCP server process itself runs as the workspace owner, not root.
proc_uid="$(docker exec "$MCPod_CONTAINER" sh -c "ps -o uid= -C mcpod | head -n1" | tr -d ' \r')"
[ "$proc_uid" = "$ws_uid" ]; check "mcpod server process uid == $ws_uid" $?

# sudo really grants container root on system locations.
[ "$(bash_code 'sudo -n test -w /var/lib/apt/lists')" = "0" ]; check "sudo can write /var/lib/apt/lists" $?
[ "$(bash_code 'sudo -n test -w /usr/local/bin')" = "0" ]; check "sudo can write /usr/local/bin" $?

# Optional apt round-trip (needs network): MCPOD_ACCEPTANCE_APT=1
if [ "${MCPOD_ACCEPTANCE_APT:-0}" = "1" ]; then
  [ "$(bash_code 'sudo -n apt-get update -qq >/dev/null && sudo -n apt-get install -y --no-install-recommends shellcheck >/dev/null && shellcheck --version >/dev/null')" = "0" ]
  check "sudo apt-get install shellcheck -> immediately usable" $?
  mcp_bash 'sudo -n apt-get purge -y shellcheck >/dev/null 2>&1 || true' >/dev/null
fi

# 15. File ownership: every agent-created file keeps the workspace owner.
# Container-side stat covers bind mounts (numeric uid is the host uid) and
# named volumes alike; host-side stat is added when the source is a real dir.
cstat() { docker exec "$MCPod_CONTAINER" stat -c '%u:%g' "$1" | tr -d '\r'; }

post '{"jsonrpc":"2.0","id":20,"method":"tools/call","params":{"name":"write","arguments":{"path":"ownership-write.txt","content":"mcpod ownership write"}}}' >/dev/null
[ "$(cstat "$ws_path/ownership-write.txt")" = "$ws_owner" ]; check "write tool: new file owned by $ws_owner" $?

bash_code 'printf "mcpod ownership edit me\n" > ownership-edit.txt' >/dev/null
[ "$(cstat "$ws_path/ownership-edit.txt")" = "$ws_owner" ]; check "pre-edit file owned by $ws_owner" $?
post '{"jsonrpc":"2.0","id":21,"method":"tools/call","params":{"name":"edit","arguments":{"path":"ownership-edit.txt","edits":[{"oldText":"ownership","newText":"ownership-v2"}]}}}' >/dev/null
[ "$(cstat "$ws_path/ownership-edit.txt")" = "$ws_owner" ]; check "edit atomic rename keeps owner $ws_owner" $?

bash_code 'touch ownership-bash.txt && mkdir -p ownership-dir' >/dev/null
[ "$(cstat "$ws_path/ownership-bash.txt")" = "$ws_owner" ]; check "bash touch keeps owner $ws_owner" $?
[ "$(cstat "$ws_path/ownership-dir")" = "$ws_owner" ]; check "bash mkdir keeps owner $ws_owner" $?

# Explicit sudo in the workspace creates root-owned files — expected Linux
# semantics, proving sudo is genuinely elevated.
if [ "$ws_uid" != "0" ]; then
  bash_code 'sudo -n touch ownership-root.txt' >/dev/null
  [ "$(cstat "$ws_path/ownership-root.txt")" = "0:0" ]; check "sudo touch -> root-owned (expected)" $?
  bash_code 'sudo -n rm -f ownership-root.txt' >/dev/null
fi

# Host-side verification when the mount source is a real host directory.
ws_src="$(docker inspect "$MCPod_CONTAINER" --format "{{range .Mounts}}{{if eq .Destination \"$ws_path\"}}{{.Source}}{{end}}{{end}}")"
if [ -n "$ws_src" ] && [ -d "$ws_src" ]; then
  host_owner() { # portable stat: GNU (-c) and BSD/macOS (-f)
    if stat -c '%u:%g' "$1" >/dev/null 2>&1; then stat -c '%u:%g' "$1"
    else stat -f '%u:%g' "$1"; fi
  }
  expected_host="$(host_owner "$ws_src")"
  [ "$(host_owner "$ws_src/ownership-write.txt")" = "$expected_host" ]; check "host: write file owner == $expected_host" $?
  [ "$(host_owner "$ws_src/ownership-edit.txt")" = "$expected_host" ]; check "host: edit file owner == $expected_host" $?
  [ "$(host_owner "$ws_src/ownership-bash.txt")" = "$expected_host" ]; check "host: bash file owner == $expected_host" $?
  [ "$(host_owner "$ws_src/ownership-dir")" = "$expected_host" ]; check "host: bash dir owner == $expected_host" $?
fi

# 16. HOME, git and mise compatibility for the dynamic identity.
[ "$(bash_code 'mkdir -p ~/.ssh && touch ~/.ssh/config && test -f ~/.ssh/config')" = "0" ]; check "~/.ssh is writable" $?
[ "$(bash_code 'touch "$HOME/.mcpod-acceptance" && rm "$HOME/.mcpod-acceptance"')" = "0" ]; check "HOME is writable" $?
[ "$(bash_out 'python -c "import os; print(os.path.expanduser(\"~\"))"')" = "$mcpod_home" ]; check "python expanduser ~ -> $mcpod_home" $?

bash_code 'git config --global user.name "MCPod Test" && git config --global user.email mcpod@example.invalid' >/dev/null
[ "$(bash_out 'git config --global --get user.name')" = "MCPod Test" ]; check "git --global config writes \$HOME/.gitconfig" $?
git_cmd='mkdir -p ownership-git && cd ownership-git && git init -q . && git config user.name "MCPod Test" && git config user.email mcpod@example.invalid && echo hello > a.txt && git add a.txt && git commit -qm "ownership test" && git status --porcelain'
[ "$(bash_code "$git_cmd")" = "0" ]; check "git init/add/commit/status in workspace" $?

[ "$(bash_code 'mise --version')" = "0" ]; check "mise --version as runtime user" $?
[ "$(bash_code 'python --version && pip --version && ruff --version && pytest --version')" = "0" ]; check "python/pip/ruff/pytest resolve" $?
[ "$(bash_code 'mise which python')" = "0" ]; check "mise resolves the preinstalled python" $?

# Optional user-level mise install round-trip (needs network): MCPOD_ACCEPTANCE_MISE=1
if [ "${MCPOD_ACCEPTANCE_MISE:-0}" = "1" ]; then
  # `mise use -g` installs AND activates (config + shims), unlike bare install.
  [ "$(bash_code 'mise use -g eza@latest >/dev/null 2>&1 && eza --version >/dev/null')" = "0" ]
  check "mise use -g as unprivileged user -> immediately usable" $?
  mcp_bash 'mise rm -g eza >/dev/null 2>&1 || true; mise uninstall eza@latest >/dev/null 2>&1 || true' >/dev/null
fi

# Cleanup ownership test artifacts (keep acceptance.txt like section 5-6 did).
bash_code 'rm -rf ownership-write.txt ownership-edit.txt ownership-bash.txt ownership-dir ownership-git' >/dev/null

echo
echo "passed=$pass failed=$fail"
[ "$fail" = "0" ]
