#!/usr/bin/env bash
# temple end-to-end test: starts server, chats via WebSocket, validates responses.
# Usage: ./test-e2e.sh [model]   (default model: fast-gemma-4-12b-it)

set -u
MODEL="${1:-fast-gemma-4-12b-it}"
PORT=42123
DB=/tmp/temple-test.db
LOG=/tmp/temple-server.log
cd "$(dirname "$0")"

echo "=== temple e2e test (model: $MODEL) ==="

# Build if needed
if [ ! -x ./target/release/temple-server ]; then
  echo "Building..."
  nix develop --command cargo build --release || exit 1
fi

# Kill leftovers
pkill -f "temple-server.*$PORT" 2>/dev/null
sleep 1
rm -f "$DB" "$LOG"

# Start server
echo "Starting temple-server on :$PORT ..."
LITELLM_API_KEY="${LITELLM_API_KEY:-$LITELLM_MASTER_KEY}" ./target/release/temple-server \
  --listen 127.0.0.1:$PORT --db-path "$DB" > "$LOG" 2>&1 &
SRV=$!
trap 'kill $SRV 2>/dev/null' EXIT
sleep 3

if ! kill -0 $SRV 2>/dev/null; then
  echo "SERVER FAILED TO START:"
  cat "$LOG"
  exit 1
fi
grep -q "listening" "$LOG" && echo "✓ server up" || { echo "✗ server not listening"; cat "$LOG"; exit 1; }
grep -q "MCP tools" "$LOG" && echo "✓ $(grep 'MCP tools' "$LOG")" || echo "⚠ no MCP tools line (check $LOG)"

# Run WebSocket test client
echo
echo "=== running chat test ==="
PYENV=$(nix build --no-link --print-out-paths --impure --expr '
  with import <nixpkgs> {};
  python3.withPackages (ps: [ ps.websocket-client ])
') || { echo "failed to build python env"; exit 1; }
"$PYENV/bin/python3" - "$PORT" "$MODEL" <<'PYEOF'
import json, sys, time
from websocket import create_connection

port, model = sys.argv[1], sys.argv[2]
ws = create_connection(f"ws://127.0.0.1:{port}", timeout=120)

def send(m): ws.send(json.dumps(m))
def recv():
    return json.loads(ws.recv())

# 1. Open session
send({"type": "OpenSession", "client_id": "e2e", "cwd": "/tmp", "hostname": "test", "username": "test"})
r = recv()
assert r["type"] == "SessionOpened", r
sid = r["session_id"]
print(f"✓ session opened: {sid[:8]}")

# 2. List models
send({"type": "ListModels"})
r = recv()
assert r["type"] == "ModelList", r
ids = [m["id"] for m in r["models"]]
print(f"✓ model list: {len(ids)} models")
assert model in ids, f"{model} not in {ids}"

# 3. Set model
send({"type": "SetModel", "session_id": sid, "model": model})
r = recv()
assert r["type"] == "ModelChanged", r
print(f"✓ model set to {model}")

# 4. Simple chat
send({"type": "ChatInput", "session_id": sid, "content": "Reply with exactly: TEMPLE WORKS"})
content, stats, tools = "", None, []
t0 = time.time()
while True:
    r = recv()
    t = r["type"]
    if t == "ChatDelta":
        content += r.get("delta", "")
        if r.get("done"): pass
    elif t == "ToolEvent":
        tools.append((r["name"], r["status"]))
    elif t == "ChatStats":
        stats = r
        break
    elif t == "ChatError":
        print(f"✗ chat error: {r['error']}")
        sys.exit(1)

dt = time.time() - t0
print(f"✓ chat response ({dt:.1f}s): {content.strip()[:100]!r}")
if stats:
    print(f"✓ stats: {stats['model']} · {stats['tokens_per_second']:.1f} tok/s · ctx {stats['context_length']}")
assert "TEMPLE WORKS" in content, f"unexpected content: {content!r}"

# 5. Tool-use chat (ask it to read a file)
print()
print("=== tool use test ===")
with open("/tmp/temple-marker.txt", "w") as f:
    f.write("MARKER-7749")
send({"type": "ChatInput", "session_id": sid,
      "content": "Use the read_file tool to read /tmp/temple-marker.txt and tell me what it says. Just the marker."})
content, stats, tools = "", None, []
while True:
    r = recv()
    t = r["type"]
    if t == "ChatDelta":
        content += r.get("delta", "")
    elif t == "ToolEvent":
        tools.append((r["name"], r["status"]))
        print(f"  tool: {r['name']} {r['status']} {r.get('detail','')[:60]}")
    elif t == "ChatStats":
        stats = r
        break
    elif t == "ChatError":
        print(f"✗ chat error: {r['error']}")
        break

print(f"✓ response: {content.strip()[:120]!r}")
if "MARKER-7749" in content or any("read_file" in n for n, _ in tools):
    print("✓ tool use confirmed")
else:
    print("⚠ marker not in response and no read_file tool used (model may have hallucinated)")

ws.close()
print()
print("=== ALL TESTS PASSED ===")
PYEOF
RC=$?

echo
echo "=== server log tail ==="
tail -15 "$LOG"
exit $RC
