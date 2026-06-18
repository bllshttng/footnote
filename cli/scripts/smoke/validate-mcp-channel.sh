#!/usr/bin/env bash
# validate-mcp-channel.sh — pin the MCP claude/channel wire format.
#
# Wave 1.0 of Phase 5 (Locked Decision 8). Validates that the envelope
# emitted by ``fno.mcp.channel.build_channel_notification`` matches
# the pinned structural shape from
# ``cli/tests/fixtures/mcp_channel_envelope.json`` (and therefore
# matches what ``channels-reference.md`` §Notification format specifies).
#
# This is a SELF-TEST of our envelope builder, NOT a Claude-mediated
# capture. Per spec: "Wave 1.0 still runs (smoke validate, not discover)
# to confirm the pinned wire format works in this user's installed CC
# version; the smoke test FAILS LOUDLY on drift. The pinned format
# comes from channels-reference.md §Notification format, not from
# runtime capture."
#
# Gated behind MCP_SMOKE=1 so CI without the abilities CLI venv skips
# it. Humans run this after bumping the channels-reference doc (or the
# CC research-preview API) to verify the fno side still emits the
# expected shape.
#
# Companion: the actual end-to-end CC integration is tested in
# ``cli/tests/mcp/test_channel_server.py`` (Wave 3.3 integration tests).

set -euo pipefail

if [[ "${MCP_SMOKE:-0}" != "1" ]]; then
    echo "validate-mcp-channel: MCP_SMOKE not set; skipping" >&2
    exit 0
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CLI_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
FIXTURE="$CLI_ROOT/tests/fixtures/mcp_channel_envelope.json"

if [[ ! -f "$FIXTURE" ]]; then
    echo "validate-mcp-channel: fixture missing at $FIXTURE" >&2
    exit 2
fi

echo "validate-mcp-channel: fixture=$FIXTURE" >&2

# Run the validation in Python so JSON comparisons are robust. The
# Python process loads the channel module and exercises both the
# builder and the validator against the pinned fixture.
PYTHONPATH="$CLI_ROOT/src" python3 - "$FIXTURE" <<'PYEOF'
import json
import re
import sys
from pathlib import Path

fixture_path = Path(sys.argv[1])
pinned = json.loads(fixture_path.read_text(encoding="utf-8"))

from fno.mcp.channel import (
    ENVELOPE_VERSION,
    META_KEY_RE,
    MCP_CHANNEL_METHOD,
    MCPChannelEnvelopeError,
    build_channel_notification,
    validate_envelope,
    envelope_drift_diff,
)

errors = []


def check(cond, msg):
    if not cond:
        errors.append(msg)


# Version pin.
check(
    ENVELOPE_VERSION == pinned["envelope_version"],
    f"ENVELOPE_VERSION drift: module={ENVELOPE_VERSION!r} fixture={pinned['envelope_version']!r}",
)

# Method pin.
check(
    MCP_CHANNEL_METHOD == pinned["method"],
    f"method drift: module={MCP_CHANNEL_METHOD!r} fixture={pinned['method']!r}",
)

# Meta key pattern pin.
check(
    META_KEY_RE.pattern == pinned["meta_key_pattern"],
    f"meta_key_pattern drift: module={META_KEY_RE.pattern!r} fixture={pinned['meta_key_pattern']!r}",
)

# Top-level + params keys check via the builder.
env = build_channel_notification(content="probe", meta={"severity": "high"})
actual_top = sorted(env.keys())
expected_top = sorted(pinned["top_level_keys"])
check(
    actual_top == expected_top,
    f"top-level keys drift: built={actual_top!r} pinned={expected_top!r}",
)

actual_params = sorted(env["params"].keys())
expected_params = sorted(pinned["params_keys"])
check(
    actual_params == expected_params,
    f"params keys drift: built={actual_params!r} pinned={expected_params!r}",
)

check(
    env["jsonrpc"] == pinned["jsonrpc"],
    f"jsonrpc drift: built={env['jsonrpc']!r} pinned={pinned['jsonrpc']!r}",
)
check(
    env["method"] == pinned["method"],
    f"method drift: built={env['method']!r} pinned={pinned['method']!r}",
)

# Validate the pinned example envelopes round-trip True.
for name in ("example_minimal", "example_with_meta"):
    ex = pinned[name]
    ok, reason = validate_envelope(ex)
    check(ok, f"fixture {name} failed validation: {reason!r}")

# Builder rejects hyphenated meta keys (the silent-drop case).
try:
    build_channel_notification(content="x", meta={"chat-id": "abc"})
    errors.append("builder accepted hyphen meta key (should raise MCPChannelEnvelopeError)")
except MCPChannelEnvelopeError:
    pass

# Validator catches a missing 'content' field.
bad = {"jsonrpc": "2.0", "method": MCP_CHANNEL_METHOD, "params": {"meta": {}}}
ok, reason = validate_envelope(bad)
check(not ok and reason == "content_missing", f"validator did not flag missing content: ok={ok!r} reason={reason!r}")

# Drift diff identifies a renamed key on receipt.
renamed = {"jsonrpc": "2.0", "method": MCP_CHANNEL_METHOD, "params": {"body": "x", "meta": {}}}
diff = envelope_drift_diff(pinned["example_minimal"], renamed)
check(
    "params_missing" in diff and "content" in diff.get("params_missing", []),
    f"drift diff did not flag renamed content key: diff={diff!r}",
)

if errors:
    print("validate-mcp-channel: FAILED", file=sys.stderr)
    for e in errors:
        print(f"  - {e}", file=sys.stderr)
    sys.exit(1)

print("validate-mcp-channel: OK (envelope version=" + ENVELOPE_VERSION + ")")
PYEOF
