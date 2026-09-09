#!/usr/bin/env bash
# x-5aef task 3.1: the auditor's reproduction, promoted to a rerunnable
# probe. Seeds an isolated store with one synthetic v2 receipt carrying
# only `active-surface=confirmed-removed` - the incomplete evidence the
# retirement gate used to certify - stamped with the CURRENT build, runs
# `reap --verify`, and requires the refusal. A pass prints the refusal
# reason it observed; exit 0. If the gate accepts the receipt, exit 1 with
# the observed JSON: the gate certifies an incomplete retirement again.
#
# Read-only against real state: everything lands in a mktemp home, never
# the operator's ~/.fno. Idempotent by construction.
#
# Usage: bash scripts/probes/retirement-gate-refuses-incomplete.sh [path-to-fno-agents]
# The binary defaults to `fno-agents` on PATH; pass the worktree-built
# binary to probe a build that is not deployed yet.

set -u

BIN="${1:-${FNO_AGENTS_BIN:-fno-agents}}"
if ! command -v "$BIN" >/dev/null 2>&1 && [ ! -x "$BIN" ]; then
    echo "probe: no fno-agents binary at '$BIN' (build one: cargo build -p fno-agents --release)" >&2
    exit 2
fi

# The receipt must carry the CURRENT build's pin, or the verifier skips it
# as stale and the probe proves nothing. Read the pin off the binary's own
# verify output - the same quantity the gate compares.
probe_home="$(mktemp -d)"
cleanup() { rm -rf "$probe_home"; }
trap cleanup EXIT
if ! build_json="$(FNO_AGENTS_HOME="$probe_home" "$BIN" reap --verify --since 1h --json)"; then
    # An empty isolated store exits 1 by design (no evidence); the JSON
    # still names the build.
    :
fi
build="$(printf '%s' "$build_json" | /usr/bin/python3 -c 'import json,sys; print(json.load(sys.stdin)["build"])')"
if [ -z "$build" ]; then
    echo "probe: could not read the build pin from '$BIN' reap --verify" >&2
    exit 2
fi

now="$(date -u +%Y-%m-%dT%H:%M:%S+00:00)"
mkdir -p "$probe_home/reap-receipts"
cat > "$probe_home/reap-receipts/claude-probe-synthetic-0001.json" <<RECEIPT
{
  "row_name": "probe-synthetic",
  "short_id": "probe-synthetic",
  "harness": "claude",
  "harness_session_id": "probe-synthetic-0001",
  "cwd": "/tmp/nowhere",
  "created_at": "$now",
  "reaped_at": "$now",
  "resume": "claude --resume probe-synthetic-0001",
  "schema_version": 2,
  "writer_build": "$build",
  "effects": [
    {
      "op": "active-surface",
      "outcome": "confirmed-removed",
      "detail": null,
      "at": "$now"
    }
  ]
}
RECEIPT

if out="$(FNO_AGENTS_HOME="$probe_home" "$BIN" reap --verify --since 24h --json)"; then
    echo "probe: GATE FAILED - the verifier accepted the incomplete receipt:" >&2
    echo "$out" >&2
    exit 1
fi
refusal="$(printf '%s' "$out" | /usr/bin/python3 -c 'import json,sys
report = json.load(sys.stdin)
for problem in report.get("problems", []):
    reason = problem.get("reason", "")
    if "required effect op" in reason:
        print(reason)
        break
else:
    sys.exit(1)
')"
if [ -z "$refusal" ]; then
    echo "probe: the verifier refused for the WRONG reason (no required-effect-op line):" >&2
    echo "$out" >&2
    exit 1
fi
echo "probe: the gate refuses the incomplete receipt, as required:"
echo "  $refusal"
exit 0
