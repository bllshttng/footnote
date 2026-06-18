#!/usr/bin/env bash
# capture-gemini-json.sh — pin the gemini -p --output-format json contract.
#
# US4-gemini Wave 2.0. Runs a real gemini invocation with --session-id
# and --output-format json, captures the JSON blob to stdout for the
# caller, and emits any structural drift to stderr.
#
# Gated behind GEMINI_SMOKE=1 so CI without a gemini binary skips it.
# CI runs this in a nightly job; humans run it manually after a gemini
# version bump to regenerate cli/tests/agents/fixtures/gemini-json-sample.json.
#
# Companion findings: cli/tests/agents/fixtures/gemini-smoke-findings.md
# documents OQ1-OQ6 resolution from the design doc.

set -euo pipefail

if [[ "${GEMINI_SMOKE:-0}" != "1" ]]; then
    echo "capture-gemini-json: GEMINI_SMOKE not set; skipping" >&2
    exit 0
fi

if ! command -v gemini >/dev/null 2>&1; then
    echo "capture-gemini-json: gemini binary not on PATH" >&2
    exit 127
fi

UUID="$(python3 -c 'import uuid; print(uuid.uuid4())')"
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT
cd "$TMPDIR"

# Empty cwd is intentional — keeps the session storage isolated from
# whatever project the operator was last in. --skip-trust suppresses
# the trusted-folders interactive prompt that breaks headless flows.
# stderr is drained to a sibling file so the JSON blob on stdout stays
# pure (gemini emits "Ripgrep is not available" + skill-conflict
# warnings to stderr that would corrupt the output if merged).
OUT_JSON="$TMPDIR/stdout.json"
gemini --skip-trust \
       -p "say only the literal word PONG, nothing else" \
       --session-id "$UUID" \
       --output-format json \
       >"$OUT_JSON" \
       2>"$TMPDIR/stderr.txt"

# Drift detection: verify the JSON has the expected top-level keys.
# Failing here means gemini renamed a key — the provider module's
# _GEMINI_KEYS constants block needs updating in lockstep.
python3 - "$UUID" "$OUT_JSON" <<'PYEOF'
import json
import sys
from pathlib import Path

expected_uuid = sys.argv[1]
data = json.loads(Path(sys.argv[2]).read_text(encoding="utf-8"))
required_keys = {"session_id", "response", "stats"}
actual_keys = set(data.keys())
missing = required_keys - actual_keys
if missing:
    print(
        f"capture-gemini-json: SCHEMA DRIFT: missing keys: {sorted(missing)} "
        f"(expected superset of {sorted(required_keys)}, "
        f"got {sorted(actual_keys)})",
        file=sys.stderr,
    )
    sys.exit(2)
if data["session_id"] != expected_uuid:
    print(
        f"capture-gemini-json: SESSION ID DRIFT: passed {expected_uuid!r}, "
        f"got back {data['session_id']!r} — Locked Decision 9 contradicted",
        file=sys.stderr,
    )
    sys.exit(3)
print("capture-gemini-json: schema OK", file=sys.stderr)
PYEOF

cat "$OUT_JSON"
