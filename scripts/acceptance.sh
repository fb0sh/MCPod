#!/usr/bin/env bash
# MCPod acceptance run (docs/plan.md §37 + pi-spec tool semantics).
# Requires a running container at 127.0.0.1:3000 and MCPOD_TOKEN exported.
set -euo pipefail

BASE=http://127.0.0.1:3000
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

echo
echo "passed=$pass failed=$fail"
[ "$fail" = "0" ]
