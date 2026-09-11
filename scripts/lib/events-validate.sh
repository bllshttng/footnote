#!/usr/bin/env bash
# scripts/lib/events-validate.sh
#
# Thin adapter over the Python validator (fno.events.validate). Python is
# the one owner of per-event validation; the hand-written bash body that
# duplicated it is retired. This adapter hands the payload to that one
# validator through the resolved interpreter and relays its verdict, so
# the shell contract below is the ONLY surface it must keep stable:
#
#   validate_event TYPE JSON_PAYLOAD
#       rc=0  valid
#       rc=1  invalid (diagnostic names the failed field on stderr)
#       rc=2  substrate failure (checkout not found, Python missing,
#             schema unavailable, payload not one JSON object)
#
# Compatibility:
#   - bash 3.2 (macOS default). No associative arrays, no process
#     substitution.
#   - The interpreter resolves through scripts/lib/fno-python.sh: a
#     linked worktree has cli/src but no cli/.venv, so the canonical
#     checkout's venv is what runs, with PYTHONPATH pinned to THIS
#     checkout's cli/src so the source under test is what validates.

set -uo pipefail

validate_event() {
    local type="${1:?type required}"
    local payload="${2:?payload required}"

    local lib_dir root
    lib_dir="$(cd "$(dirname "${BASH_SOURCE[0]:-}")" 2>/dev/null && pwd)"
    root="$(cd "$lib_dir/../.." 2>/dev/null && pwd)"
    if [[ -z "$lib_dir" || -z "$root" || ! -f "$root/cli/src/fno/events/__init__.py" ]]; then
        printf '%s\n' \
            "validate-event: cannot locate the fno checkout above ${BASH_SOURCE[0]:-this lib}" >&2
        return 2
    fi

    if [[ -z "${FNO_PYTHON:-}" ]]; then
        # shellcheck source=lib/fno-python.sh
        source "$lib_dir/fno-python.sh"
        fno_python_init "$root"
    fi
    if [[ -z "${FNO_PYTHON:-}" ]]; then
        printf '%s\n' "validate-event: no usable Python resolved for the validator" >&2
        return 2
    fi
    if [[ "$FNO_PYTHON" != "python3" && ! -x "$FNO_PYTHON" ]]; then
        printf '%s\n' "validate-event: resolved interpreter is not executable: $FNO_PYTHON" >&2
        return 2
    fi

    PYTHONPATH="$root/cli/src${PYTHONPATH:+:$PYTHONPATH}" "$FNO_PYTHON" -c '
import json, sys
from fno.events import SchemaUnavailableError, ValidationError, validate
try:
    event = json.load(sys.stdin)
except json.JSONDecodeError as exc:
    print("validate-event: payload is not valid JSON:", exc, file=sys.stderr)
    sys.exit(2)
if not isinstance(event, dict):
    print("validate-event: payload must be a JSON object", file=sys.stderr)
    sys.exit(2)
if event.get("type") != sys.argv[1]:
    print("validate-event: type hint does not match payload type:", event.get("type"), file=sys.stderr)
    sys.exit(1)
try:
    validate(event)
except SchemaUnavailableError as exc:
    print("validate-event: schema unavailable:", exc, file=sys.stderr)
    sys.exit(2)
except ValidationError as exc:
    print("validate-event:", exc, file=sys.stderr)
    sys.exit(1)
sys.exit(0)
' "$type" <<<"$payload"
}
