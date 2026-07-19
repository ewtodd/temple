#!/usr/bin/env bash
# temple chat test against any model: starts server, runs MCP tool-use chat.
# Usage: ./test-chat.sh <model> [timeout_seconds]
set -u
MODEL="${1:?usage: ./test-chat.sh <model> [timeout]}"
TIMEOUT="${2:-150}"
PORT=42123
DB=/tmp/temple-test.db
LOG=/tmp/temple-server.log
cd "$(dirname "$0")"

pkill -f "temple-server.*$PORT" 2>/dev/null
sleep 1
rm -f "$DB" "$LOG"

LITELLM_API_KEY="${LITELLM_API_KEY:-$LITELLM_MASTER_KEY}" ./target/release/temple-server \
  --listen 127.0.0.1:$PORT --db-path "$DB" > "$LOG" 2>&1 &
SRV=$!
trap 'kill $SRV 2>/dev/null' EXIT
sleep 3
grep -q "listening" "$LOG" || { echo "SERVER FAILED"; cat "$LOG"; exit 1; }
echo "server up"

PYENV=$(nix build --no-link --print-out-paths --impure --expr '
  with import <nixpkgs> {}; python3.withPackages (ps: [ ps.websocket-client ])' 2>/dev/null)

timeout "$TIMEOUT" "$PYENV/bin/python3" test-chat.py "$PORT" "$MODEL"
RC=$?
echo "--- server log (last 20, no pool spam) ---"
grep -vE "hyper|pool" "$LOG" | tail -20
exit $RC
