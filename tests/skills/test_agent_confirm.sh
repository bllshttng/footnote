#!/usr/bin/env bash
# test_agent_confirm.sh - MOVED (ab-994222ee).
#
# The confirm-posture harness now lives co-located with the skill at
# skills/agent/tests/test_confirm.sh, matching the design's verify command and
# the skill self-containment convention, AND it encodes the new free-lane
# semantics (spawn does NOT confirm by default; only config.agents.confirm:
# always opts back in; caveats become warnings). This shim runs the canonical
# harness so any existing caller stays green.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec bash "$HERE/../../skills/agent/tests/test_confirm.sh"
