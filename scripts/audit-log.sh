#!/usr/bin/env bash
# scripts/audit-log.sh — centralized audit logging for CI
# Usage: audit-log LEVEL STEP "message"
# Example: audit-log INFO lint "eslint passed"

set -euo pipefail

AUDIT_LOG="${AUDIT_LOG:-audit.log}"

log() {
    local level="$1"
    local step="$2"
    shift 2
    local msg="$*"
    local ts
    ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
    echo "[${ts}] [${level}] [${step}] ${msg}" >> "$AUDIT_LOG"
    echo "[${ts}] [${level}] [${step}] ${msg}"
}

if [ $# -lt 3 ]; then
    echo "Usage: $0 LEVEL STEP \"message\"" >&2
    exit 1
fi

log "$1" "$2" "$3"
