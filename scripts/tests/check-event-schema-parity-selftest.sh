#!/usr/bin/env bash
# Self-test for scripts/check-event-schema-parity.sh
#
# Feeds the parity script synthetic fixtures via --test-schema-dir,
# --test-python-schema, and --test-rust-schema flags and asserts
# non-zero exit on:
#   1. A drifted Branch A property (field present in Python emit-schema but
#      not in the on-disk events-v3.json Branch A)
#   2. A name collision between Python event_types and Rust event_kinds
#   3. A malformed events-v3.json schema file
#
# Also verifies exit 0 (parity OK) when fixtures are clean.
#
# Usage: bash scripts/tests/check-event-schema-parity-selftest.sh
# Exit 0 = all assertions passed; non-zero = at least one failed.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PARITY_SCRIPT="$REPO_ROOT/scripts/check-event-schema-parity.sh"

if [[ ! -x "$PARITY_SCRIPT" ]]; then
    echo "ERROR: parity script not executable: $PARITY_SCRIPT" >&2
    exit 1
fi

TMPDIR_BASE="$(mktemp -d)"
trap 'rm -rf "$TMPDIR_BASE"' EXIT

# ---------------------------------------------------------------------------
# Helper: write a minimal clean events-v3.json into a temp dir
# ---------------------------------------------------------------------------
make_events_v3() {
    local dir="$1"
    cat > "$dir/events-v3.json" << 'EOF'
{
  "$comment": "test fixture",
  "oneOf": [
    {
      "type": "object",
      "required": ["ts", "type", "source", "data"],
      "properties": {
        "ts": {"type": "string"},
        "type": {"type": "string"},
        "source": {"type": "string", "enum": ["target", "test"]},
        "data": {"type": "object"}
      },
      "not": {"required": ["kind"]},
      "additionalProperties": true
    },
    {
      "type": "object",
      "required": ["ts", "kind", "source"],
      "properties": {
        "ts": {"type": "string"},
        "kind": {"type": "string"},
        "source": {"type": "string", "pattern": "^(daemon|worker:.+)$"}
      },
      "not": {"required": ["type"]},
      "additionalProperties": true
    }
  ]
}
EOF
}

# ---------------------------------------------------------------------------
# Helper: write a minimal clean status-v1.json into a temp dir
# ---------------------------------------------------------------------------
make_status_v1() {
    local dir="$1"
    cat > "$dir/status-v1.json" << 'EOF'
{
  "type": "object",
  "required": ["schema_version", "short_id", "status"],
  "properties": {
    "schema_version": {"type": "integer"},
    "short_id": {"type": "string"},
    "status": {"type": "string", "enum": ["spawning","ready","idle","busy","live","restarting","orphaned","failed","exited","permanent_dead"]},
    "ready": {"type": "boolean"},
    "last_message_at": {"type": ["string","null"]},
    "last_reply": {"type": ["string","null"]},
    "restart_count": {"type": "integer"},
    "last_restart_at": {"type": ["string","null"]},
    "pty": {"oneOf": [{"type": "null"}, {"type": "object"}]}
  },
  "additionalProperties": false
}
EOF
}

PASS=0
FAIL=0

check() {
    local desc="$1"
    local expected_exit="$2"
    shift 2
    local actual_exit=0
    "$@" > /dev/null 2>&1 || actual_exit=$?
    if [[ "$actual_exit" -eq "$expected_exit" ]]; then
        echo "PASS: $desc (exit $actual_exit)"
        PASS=$((PASS + 1))
    else
        echo "FAIL: $desc (expected exit $expected_exit, got $actual_exit)"
        FAIL=$((FAIL + 1))
    fi
}

# ---------------------------------------------------------------------------
# Test 1: Clean fixtures -> parity OK (exit 0)
# ---------------------------------------------------------------------------
T1="$TMPDIR_BASE/t1"
mkdir "$T1"
make_events_v3 "$T1"
make_status_v1 "$T1"

check "clean fixtures -> exit 0" 0 \
    bash "$PARITY_SCRIPT" \
        --test-schema-dir "$T1" \
        --test-python-schema '{"envelope":{"type":"object","required":["ts","type","source","data"],"properties":{"ts":{"type":"string"},"type":{"type":"string"},"source":{"type":"string","enum":["target","test"]},"data":{"type":"object"}},"not":{"required":["kind"]},"additionalProperties":true},"event_types":["phase_transition"]}' \
        --test-rust-schema '{"envelope":{"type":"object","required":["ts","kind","source"],"properties":{"ts":{"type":"string"},"kind":{"type":"string"},"source":{"type":"string","pattern":"^(daemon|worker:.+)$"}},"not":{"required":["type"]},"additionalProperties":true},"status":{"type":"object","required":["schema_version","short_id","status"],"properties":{"schema_version":{"type":"integer"},"short_id":{"type":"string"},"status":{"type":"string","enum":["spawning","ready","idle","busy","live","restarting","orphaned","failed","exited","permanent_dead"]},"ready":{"type":"boolean"},"last_message_at":{"type":["string","null"]},"last_reply":{"type":["string","null"]},"restart_count":{"type":"integer"},"last_restart_at":{"type":["string","null"]},"pty":{"oneOf":[{"type":"null"},{"type":"object"}]}},"additionalProperties":false},"event_kinds":["daemon_started","agent_spawned"]}'

# ---------------------------------------------------------------------------
# Test 2: Name collision (Python event_type = Rust event_kind) -> non-zero
# ---------------------------------------------------------------------------
T2="$TMPDIR_BASE/t2"
mkdir "$T2"
make_events_v3 "$T2"
make_status_v1 "$T2"

check "name collision -> non-zero" 1 \
    bash "$PARITY_SCRIPT" \
        --test-schema-dir "$T2" \
        --test-python-schema '{"envelope":{"type":"object","required":["ts","type","source","data"],"properties":{"ts":{"type":"string"},"type":{"type":"string"},"source":{"type":"string","enum":["target","test"]},"data":{"type":"object"}},"not":{"required":["kind"]},"additionalProperties":true},"event_types":["phase_transition","agent_spawned"]}' \
        --test-rust-schema '{"envelope":{"type":"object","required":["ts","kind","source"],"properties":{"ts":{"type":"string"},"kind":{"type":"string"},"source":{"type":"string","pattern":"^(daemon|worker:.+)$"}},"not":{"required":["type"]},"additionalProperties":true},"status":{"type":"object","required":["schema_version","short_id","status"],"properties":{"schema_version":{"type":"integer"},"short_id":{"type":"string"},"status":{"type":"string","enum":["spawning","ready","idle","busy","live","restarting","orphaned","failed","exited","permanent_dead"]},"ready":{"type":"boolean"},"last_message_at":{"type":["string","null"]},"last_reply":{"type":["string","null"]},"restart_count":{"type":"integer"},"last_restart_at":{"type":["string","null"]},"pty":{"oneOf":[{"type":"null"},{"type":"object"}]}},"additionalProperties":false},"event_kinds":["agent_spawned","daemon_started"]}'

# ---------------------------------------------------------------------------
# Test 3: Drifted Python envelope (extra field in emitted schema not in on-disk)
# ---------------------------------------------------------------------------
T3="$TMPDIR_BASE/t3"
mkdir "$T3"
make_events_v3 "$T3"
make_status_v1 "$T3"

check "drifted python envelope -> non-zero" 1 \
    bash "$PARITY_SCRIPT" \
        --test-schema-dir "$T3" \
        --test-python-schema '{"envelope":{"type":"object","required":["ts","type","source","data","DRIFTED_FIELD"],"properties":{"ts":{"type":"string"},"type":{"type":"string"},"source":{"type":"string","enum":["target","test"]},"data":{"type":"object"},"DRIFTED_FIELD":{"type":"string"}},"not":{"required":["kind"]},"additionalProperties":true},"event_types":["phase_transition"]}' \
        --test-rust-schema '{"envelope":{"type":"object","required":["ts","kind","source"],"properties":{"ts":{"type":"string"},"kind":{"type":"string"},"source":{"type":"string","pattern":"^(daemon|worker:.+)$"}},"not":{"required":["type"]},"additionalProperties":true},"status":{"type":"object","required":["schema_version","short_id","status"],"properties":{"schema_version":{"type":"integer"},"short_id":{"type":"string"},"status":{"type":"string","enum":["spawning","ready","idle","busy","live","restarting","orphaned","failed","exited","permanent_dead"]},"ready":{"type":"boolean"},"last_message_at":{"type":["string","null"]},"last_reply":{"type":["string","null"]},"restart_count":{"type":"integer"},"last_restart_at":{"type":["string","null"]},"pty":{"oneOf":[{"type":"null"},{"type":"object"}]}},"additionalProperties":false},"event_kinds":["daemon_started","agent_spawned"]}'

# ---------------------------------------------------------------------------
# Test 4: Malformed JSON schema -> non-zero
# ---------------------------------------------------------------------------
T4="$TMPDIR_BASE/t4"
mkdir "$T4"
printf 'NOT JSON' > "$T4/events-v3.json"
make_status_v1 "$T4"

check "malformed events-v3.json -> non-zero" 1 \
    bash "$PARITY_SCRIPT" \
        --test-schema-dir "$T4" \
        --test-python-schema '{"envelope":{},"event_types":[]}' \
        --test-rust-schema '{"envelope":{},"status":{},"event_kinds":[]}'

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "Results: $PASS passed, $FAIL failed"
if [[ "$FAIL" -gt 0 ]]; then
    exit 1
fi
exit 0
