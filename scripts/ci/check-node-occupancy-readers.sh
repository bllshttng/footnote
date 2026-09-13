#!/usr/bin/env bash
# check-node-occupancy-readers.sh - gate the node-claim reader census (x-74aa).
#
# The self-test plants a known-bad site and proves the detector detects it
# before this gate reports clean; then every node-claim occupancy read under
# cli/src/fno must be registered in the allowlist with its covering control.
# Stdlib-only detector: no fno import, so bare python3 works hermetically.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TOOL="$ROOT/scripts/diagnostics/occupancy-readers.py"

python3 "$TOOL" --self-test || {
    echo "check-node-occupancy-readers: self-test did not pass" >&2
    exit 1
}
python3 "$TOOL"
echo "check-node-occupancy-readers: OK"
