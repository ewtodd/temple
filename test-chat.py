import json, sys, time
from websocket import create_connection

port, model = int(sys.argv[1]), sys.argv[2]
ws = create_connection(f"ws://127.0.0.1:{port}", timeout=300)
def send(m): ws.send(json.dumps(m))
def recv(): return json.loads(ws.recv())

send({"type": "OpenSession", "client_id": "e2e", "cwd": "/tmp", "hostname": "test", "username": "test"})
r = recv()
assert r["type"] == "SessionOpened", r
sid = r["session_id"]
send({"type": "SetModel", "session_id": sid, "model": model})
r = recv()
assert r["type"] == "ModelChanged", r
print(f"model={model}", flush=True)

send({"type": "ChatInput", "session_id": sid,
      "content": "Use the searxng-web_search tool to search for 'NixOS'. Then give a one-line summary."})
tools = []
content = ""
t0 = time.time()
while True:
    r = recv()
    t = r["type"]
    if t == "ChatDelta":
        content += r.get("delta", "")
    elif t == "ToolEvent":
        tools.append((r["name"], r["status"]))
        print("  tool: {} {}".format(r["name"], r["status"]), flush=True)
    elif t == "ChatStats":
        print("  stats: {:.1f} tok/s, ctx {}, {}ms".format(
            r["tokens_per_second"], r["context_length"], r["duration_ms"]), flush=True)
        break
    elif t == "ChatError":
        print("  ERROR: {}".format(r["error"][:300]), flush=True)
        sys.exit(1)

dt = time.time() - t0
print("  tools: {}".format([n for n, _ in tools]), flush=True)
print("  response ({:.1f}s): {!r}".format(dt, content.strip()[:300]), flush=True)
ws.close()
print("PASS" if content.strip() else "EMPTY RESPONSE")
