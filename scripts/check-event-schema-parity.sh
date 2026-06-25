#!/usr/bin/env bash
# scripts/check-event-schema-parity.sh
#
# Cross-language event schema parity check (W7).
#
# 1. Validates events-v3.json + status-v1.json parse as JSON.
# 2. Runs `python -m fno.events --emit-schema` (30s timeout).
# 3. Runs `fno-agents --emit-schema` if the binary is found (30s timeout);
#    WARN + continue (exit 0) if absent (the Rust CI job is the real gate).
# 4. Diffs each emitter's self-described envelope against the on-disk schema.
# 5. Asserts global uniqueness of event names across both languages.
# 6. Prints "parity OK" and exits 0 on all-pass.
#
# Test mode: pass --test-schema-dir, --test-python-schema, --test-rust-schema
# to inject synthetic fixtures without touching production paths.
#
# Exit codes:
#   0  all checks pass (or Rust binary absent + WARN)
#   1  parity failure (drift, collision, malformed schema, emitter error)
#   2  usage error
set -euo pipefail

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------
TEST_SCHEMA_DIR=""
TEST_PYTHON_SCHEMA=""
TEST_RUST_SCHEMA=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --test-schema-dir)
            TEST_SCHEMA_DIR="$2"; shift 2 ;;
        --test-python-schema)
            TEST_PYTHON_SCHEMA="$2"; shift 2 ;;
        --test-rust-schema)
            TEST_RUST_SCHEMA="$2"; shift 2 ;;
        *)
            echo "check-event-schema-parity: unknown flag: $1" >&2
            exit 2 ;;
    esac
done

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# ---------------------------------------------------------------------------
# Python interpreter: prefer the cli venv's python (which has abilities
# installed) over the system python3. The cli venv is created by `uv sync`
# in cli/. Fall back to `python3` if the venv isn't present (e.g. CI after
# `uv sync` installs to PATH via uv's managed env or the wheel is installed
# into the system python).
# ---------------------------------------------------------------------------
if [[ -x "$REPO_ROOT/cli/.venv/bin/python" ]]; then
    PYTHON3="$REPO_ROOT/cli/.venv/bin/python"
elif [[ -x "$REPO_ROOT/cli/.venv/bin/python3" ]]; then
    PYTHON3="$REPO_ROOT/cli/.venv/bin/python3"
else
    PYTHON3="python3"
fi

# Portable timeout wrapper. Uses `timeout` (GNU coreutils, Linux + macOS
# via brew) when available; falls back to `perl -e 'alarm N; exec ...'`.
_timeout() {
    local secs="$1"; shift
    if command -v timeout &>/dev/null; then
        timeout "$secs" "$@"
    elif command -v gtimeout &>/dev/null; then
        gtimeout "$secs" "$@"
    else
        # perl alarm fallback: runs in the same process, so exec replaces perl.
        perl -e "alarm $secs; exec @ARGV or die \$!" -- "$@"
    fi
}

# ---------------------------------------------------------------------------
# Schema dir: use test override or on-disk canonical
# ---------------------------------------------------------------------------
if [[ -n "$TEST_SCHEMA_DIR" ]]; then
    SCHEMA_DIR="$TEST_SCHEMA_DIR"
else
    SCHEMA_DIR="$REPO_ROOT/schemas"
fi
EVENTS_V3="$SCHEMA_DIR/events-v3.json"
STATUS_V1="$SCHEMA_DIR/status-v1.json"

# ---------------------------------------------------------------------------
# Step 1: Validate on-disk schemas parse as JSON
# ---------------------------------------------------------------------------
echo "check-event-schema-parity: validating on-disk schemas..."

for schema_file in "$EVENTS_V3" "$STATUS_V1"; do
    if [[ ! -f "$schema_file" ]]; then
        echo "ERROR: schema file not found: $schema_file" >&2
        exit 1
    fi
    if ! "$PYTHON3" -c "import json,sys; json.load(open(sys.argv[1]))" "$schema_file" 2>/dev/null; then
        echo "ERROR: schema file is not valid JSON: $schema_file" >&2
        exit 1
    fi
done
echo "  on-disk schemas: OK"

# ---------------------------------------------------------------------------
# Python comparison helper
# ---------------------------------------------------------------------------
# compare_json <label> <got_json> <want_json>
# Uses Python to do a structural deep-equal comparison.
compare_json() {
    local label="$1"
    # JSON payloads are passed via the environment, not interpolated into the
    # Python source, so bash never expands `$`/`\` inside them and the script
    # body is a quoted heredoc (no expansion). Robust to JSON containing any
    # quote/backslash sequence (Gemini PR #354 HIGH finding).
    GOT_JSON="$2" WANT_JSON="$3" "$PYTHON3" - "$label" <<'PYEOF'
import json, os, sys

label = sys.argv[1]
got = json.loads(os.environ["GOT_JSON"])
want = json.loads(os.environ["WANT_JSON"])

# Documentation-only keys: not structural validators; ignored during comparison
# so emitters can emit minimal schemas while on-disk schemas have rich docs.
DOC_KEYS = {"description", "title", "$schema", "$comment"}

def strip_docs(obj):
    """Recursively remove documentation-only keys from a schema object."""
    if isinstance(obj, dict):
        return {k: strip_docs(v) for k, v in obj.items() if k not in DOC_KEYS}
    if isinstance(obj, list):
        return [strip_docs(x) for x in obj]
    return obj

got_s = strip_docs(got)
want_s = strip_docs(want)

def diff_path(a, b, path=""):
    if type(a) != type(b):
        return [f"{path}: type mismatch ({type(a).__name__} vs {type(b).__name__})"]
    if isinstance(a, dict):
        errs = []
        for k in set(list(a.keys()) + list(b.keys())):
            p = f"{path}.{k}" if path else k
            if k in a and k not in b:
                errs.append(f"{p}: present in emitter, absent in on-disk schema")
            elif k not in a and k in b:
                errs.append(f"{p}: absent in emitter, present in on-disk schema")
            else:
                errs += diff_path(a[k], b[k], p)
        return errs
    if a != b:
        # For lists compare as sorted sets of primitive items for readability
        if isinstance(a, list) and isinstance(b, list):
            # json.dumps(sort_keys=True) gives a stable canonical form for
            # nested objects (e.g. oneOf branches); str(dict) ordering is not
            # guaranteed stable for structural equality (Gemini PR #354).
            s_a = set(json.dumps(x, sort_keys=True) for x in a)
            s_b = set(json.dumps(x, sort_keys=True) for x in b)
            missing = s_b - s_a
            extra = s_a - s_b
            errs = []
            if missing:
                errs.append(f"{path}: missing items {sorted(missing)}")
            if extra:
                errs.append(f"{path}: extra items {sorted(extra)}")
            return errs
        return [f"{path}: {a!r} != {b!r}"]
    return []

errs = diff_path(got_s, want_s)
if errs:
    print(f"DRIFT in {label}:")
    for e in errs:
        print(f"  {e}")
    sys.exit(1)
PYEOF
}

# ---------------------------------------------------------------------------
# Step 2: Python emit-schema
# ---------------------------------------------------------------------------
echo "check-event-schema-parity: running python -m fno.events --emit-schema..."

if [[ -n "$TEST_PYTHON_SCHEMA" ]]; then
    PYTHON_SCHEMA_JSON="$TEST_PYTHON_SCHEMA"
else
    PYTHON_STDERR_TMP="$(mktemp)"
    if ! PYTHON_SCHEMA_JSON="$(
        cd "$REPO_ROOT"
        _timeout 30 "$PYTHON3" -m fno.events --emit-schema 2>"$PYTHON_STDERR_TMP"
    )"; then
        echo "ERROR: python -m fno.events --emit-schema failed" >&2
        echo "  stderr: $(head -5 "$PYTHON_STDERR_TMP")" >&2
        rm -f "$PYTHON_STDERR_TMP"
        exit 1
    fi
    rm -f "$PYTHON_STDERR_TMP"

    if ! "$PYTHON3" -c "import json,sys; json.loads(sys.argv[1])" "$PYTHON_SCHEMA_JSON" 2>/dev/null; then
        echo "ERROR: python emit-schema output is not valid JSON" >&2
        exit 1
    fi
fi
echo "  python emit-schema: OK"

# Extract python envelope and event_types
PYTHON_ENVELOPE="$("$PYTHON3" -c "import json,sys; d=json.loads(sys.argv[1]); print(json.dumps(d['envelope']))" "$PYTHON_SCHEMA_JSON")"
PYTHON_EVENT_TYPES="$("$PYTHON3" -c "import json,sys; d=json.loads(sys.argv[1]); print(json.dumps(d['event_types']))" "$PYTHON_SCHEMA_JSON")"

# ---------------------------------------------------------------------------
# Step 3: Rust emit-schema (optional -- WARN + continue if binary absent)
# ---------------------------------------------------------------------------
RUST_AVAILABLE=false
RUST_SCHEMA_JSON=""

if [[ -n "$TEST_RUST_SCHEMA" ]]; then
    RUST_SCHEMA_JSON="$TEST_RUST_SCHEMA"
    RUST_AVAILABLE=true
else
    # Find fno-agents binary on PATH or in common build outputs
    FNO_AGENTS_BIN=""
    if command -v fno-agents &>/dev/null; then
        FNO_AGENTS_BIN="$(command -v fno-agents)"
    else
        for candidate in \
            "$REPO_ROOT/crates/fno-agents/target/debug/fno-agents" \
            "$REPO_ROOT/crates/fno-agents/target/release/fno-agents"; do
            if [[ -x "$candidate" ]]; then
                FNO_AGENTS_BIN="$candidate"
                break
            fi
        done
    fi

    if [[ -z "$FNO_AGENTS_BIN" ]]; then
        echo "WARN: fno-agents binary not found on PATH or in crates/fno-agents/target/." >&2
        echo "  Skipping Rust parity check. The rust-ci.yml job is the gate that" >&2
        echo "  requires the binary to be built. Python-side checks continue." >&2
    else
        RUST_STDERR_TMP="$(mktemp)"
        if ! RUST_SCHEMA_JSON="$(
            _timeout 30 "$FNO_AGENTS_BIN" --emit-schema 2>"$RUST_STDERR_TMP"
        )"; then
            echo "ERROR: fno-agents --emit-schema failed" >&2
            echo "  binary: $FNO_AGENTS_BIN" >&2
            echo "  stderr: $(head -5 "$RUST_STDERR_TMP")" >&2
            rm -f "$RUST_STDERR_TMP"
            exit 1
        fi
        rm -f "$RUST_STDERR_TMP"

        if ! "$PYTHON3" -c "import json,sys; json.loads(sys.argv[1])" "$RUST_SCHEMA_JSON" 2>/dev/null; then
            echo "ERROR: fno-agents emit-schema output is not valid JSON" >&2
            exit 1
        fi
        RUST_AVAILABLE=true
    fi
fi

if [[ "$RUST_AVAILABLE" == "true" ]]; then
    echo "  rust emit-schema: OK"
fi

# ---------------------------------------------------------------------------
# Step 4: Diff emitters against on-disk canonical schemas
# ---------------------------------------------------------------------------
echo "check-event-schema-parity: diffing against on-disk schemas..."

# Load on-disk Branch A (Python branch)
DISK_BRANCH_A="$("$PYTHON3" -c "
import json, sys
schema = json.load(open(sys.argv[1]))
branches = schema.get('oneOf', [])
branch_a = next((b for b in branches if 'type' in b.get('required', [])), None)
if branch_a is None:
    print('ERROR: events-v3.json has no Branch A (required=[type])', file=sys.stderr)
    sys.exit(1)
print(json.dumps(branch_a))
" "$EVENTS_V3")"

# Load on-disk Branch B (Rust branch)
DISK_BRANCH_B="$("$PYTHON3" -c "
import json, sys
schema = json.load(open(sys.argv[1]))
branches = schema.get('oneOf', [])
branch_b = next((b for b in branches if 'kind' in b.get('required', [])), None)
if branch_b is None:
    print('ERROR: events-v3.json has no Branch B (required=[kind])', file=sys.stderr)
    sys.exit(1)
print(json.dumps(branch_b))
" "$EVENTS_V3")"

# Load on-disk status-v1
DISK_STATUS="$("$PYTHON3" -c "import json,sys; print(json.dumps(json.load(open(sys.argv[1]))))" "$STATUS_V1")"

DRIFT_FOUND=false

# Compare Python envelope against Branch A
if ! compare_json "python-envelope vs events-v3.json Branch A" "$PYTHON_ENVELOPE" "$DISK_BRANCH_A"; then
    DRIFT_FOUND=true
fi

# Compare Rust envelope + status against on-disk (only if Rust binary available)
if [[ "$RUST_AVAILABLE" == "true" ]]; then
    RUST_ENVELOPE="$("$PYTHON3" -c "import json,sys; d=json.loads(sys.argv[1]); print(json.dumps(d['envelope']))" "$RUST_SCHEMA_JSON")"
    RUST_STATUS="$("$PYTHON3" -c "import json,sys; d=json.loads(sys.argv[1]); print(json.dumps(d['status']))" "$RUST_SCHEMA_JSON")"

    if ! compare_json "rust-envelope vs events-v3.json Branch B" "$RUST_ENVELOPE" "$DISK_BRANCH_B"; then
        DRIFT_FOUND=true
    fi
    if ! compare_json "rust-status vs status-v1.json" "$RUST_STATUS" "$DISK_STATUS"; then
        DRIFT_FOUND=true
    fi
fi

if [[ "$DRIFT_FOUND" == "true" ]]; then
    echo "ERROR: schema drift detected (see diff above)" >&2
    exit 1
fi
echo "  drift check: OK"

# ---------------------------------------------------------------------------
# Step 5: Assert global uniqueness of event names across both languages
# ---------------------------------------------------------------------------
echo "check-event-schema-parity: checking for name collisions..."

# Write the JSON data to temp files so we avoid quoting issues with large JSON
# blobs inside a heredoc. The Python script reads them by path.
_COLLISION_TMPDIR="$(mktemp -d)"
printf '%s' "$PYTHON_EVENT_TYPES" > "$_COLLISION_TMPDIR/python_event_types.json"
if [[ "$RUST_AVAILABLE" == "true" ]]; then
    printf '%s' "$RUST_SCHEMA_JSON" > "$_COLLISION_TMPDIR/rust_schema.json"
    _RUST_AVAILABLE_FLAG="true"
else
    printf '{"event_kinds":[]}' > "$_COLLISION_TMPDIR/rust_schema.json"
    _RUST_AVAILABLE_FLAG="false"
fi

_COLLISION_STATUS=0
"$PYTHON3" - "$_COLLISION_TMPDIR" "$_RUST_AVAILABLE_FLAG" <<'PYEOF' || _COLLISION_STATUS=$?
import json, sys

tmpdir = sys.argv[1]
rust_available = sys.argv[2] == "true"

python_types = set(json.loads(open(f"{tmpdir}/python_event_types.json").read()))
if rust_available:
    rust_schema = json.loads(open(f"{tmpdir}/rust_schema.json").read())
    if "event_kinds" not in rust_schema:
        print("ERROR: Rust schema is missing required 'event_kinds' key")
        sys.exit(1)
    rust_kinds = set(rust_schema["event_kinds"])
else:
    rust_kinds = set()

collisions = python_types & rust_kinds
if collisions:
    print("COLLISION: the following event names appear in both Python and Rust:")
    for name in sorted(collisions):
        print(f"  {name}")
    sys.exit(1)
print("  no collisions")
PYEOF

rm -rf "$_COLLISION_TMPDIR"
if [[ $_COLLISION_STATUS -ne 0 ]]; then
    exit 1
fi

# ---------------------------------------------------------------------------
# All checks passed
# ---------------------------------------------------------------------------
echo ""
echo "parity OK"
exit 0
