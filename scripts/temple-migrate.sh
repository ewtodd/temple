#!/usr/bin/env bash
# temple-migrate.sh — split the shared temple-server DB into per-user
# daemon DBs for the workstation deployment.
#
# Usage (on oracle, after stopping temple-server):
#   temple-migrate.sh /var/lib/temple/temple.db /var/lib/temple e-play e-work
#
# Each user gets a full copy of the schema with only their own rows:
# sessions (the first user also keeps shared "group" sessions), their
# conversations, their memories (global/system scopes go to everyone),
# their skills, and the shared signal_users mapping. Session logs are
# copied per user the same way.
set -euo pipefail

SRC="${1:?usage: temple-migrate.sh <src.db> <dst-dir> <users...>}"
DST_DIR="${2:?usage: temple-migrate.sh <src.db> <dst-dir> <users...>}"
shift 2
USERS=("$@")
GROUP_OWNER="${USERS[0]}"

if [[ ! -f "$SRC" ]]; then
  echo "source DB not found: $SRC" >&2
  exit 1
fi

LOG_SRC="$(dirname "$SRC")/session-logs"

for u in "${USERS[@]}"; do
  dst="$DST_DIR/$u/temple.db"
  mkdir -p "$(dirname "$dst")"
  cp "$SRC" "$dst"

  # Keep only this user's sessions; the group owner also keeps shared
  # group sessions (Signal group chats).
  if [[ "$u" == "$GROUP_OWNER" ]]; then
    sqlite3 "$dst" "DELETE FROM sessions WHERE username NOT IN ('$u', 'group');"
  else
    sqlite3 "$dst" "DELETE FROM sessions WHERE username != '$u';"
  fi
  sqlite3 "$dst" "DELETE FROM conversations WHERE session_id NOT IN (SELECT id FROM sessions);"
  sqlite3 "$dst" "DELETE FROM memory_store WHERE scope NOT IN ('global', 'system', '$u');"
  sqlite3 "$dst" "DELETE FROM skills WHERE username != '$u';"
  # signal_users is the shared phone→user mapping — keep it whole.

  # Session logs: same ownership split.
  if [[ -d "$LOG_SRC" ]]; then
    log_dst="$DST_DIR/$u/session-logs"
    mkdir -p "$log_dst"
    mapfile -t keep < <(sqlite3 "$dst" "SELECT id FROM sessions;")
    for f in "$LOG_SRC"/*.jsonl; do
      [[ -e "$f" ]] || continue
      id="$(basename "$f" .jsonl)"
      for k in "${keep[@]}"; do
        if [[ "$id" == "$k" ]]; then
          cp "$f" "$log_dst/"
          break
        fi
      done
    done
  fi

  echo "wrote $dst ($(sqlite3 "$dst" 'SELECT COUNT(*) FROM sessions;') sessions)"
done

echo "done — copy $DST_DIR/<user>/ to each workstation state dir (default /var/lib/temple/<user>/)."
