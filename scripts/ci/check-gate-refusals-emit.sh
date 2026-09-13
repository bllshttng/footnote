#!/usr/bin/env bash
# A spawn-gate refusal must leave a machine-readable trace.
#
# Measured 2026-09-01: the global journal carried 4815 `claim_acquired` rows -
# the positive control that it is read and written - and zero rows of any kind
# naming a gate refusal. Meanwhile `agents.provider_limits.zai.lanes = 7` was
# binding on the live fleet. So a refusal existed only in the stderr of a
# process that had already exited, and nobody could ask why a node did not
# launch.
#
# The fix was one seam, `_refuse()`, that emits and then raises. This guard is
# what keeps it one seam: an eleventh refusal branch that raises directly would
# be silent again, and silent in exactly the way that took a whole audit to
# notice. A convention nothing checks is a convention that decays.
#
# SCOPE, stated because a partial instrument that looks complete is worse than
# an absent one. This covers the PYTHON side of the one spawn gate
# (x-6089): spawn_gate.py now holds exactly one refusal CONSTRUCTION site
# (inside _refuse) plus the transport's call into it - the gate's axes
# themselves live in crates/fno-agents/src/spawn_gate.rs and answer the
# Python transport as data, so a spawn that enters Python still emits its
# refusal through this seam, and a native (bg/headless) spawn emits nothing
# (x-ab75 owns a Rust emit).
#
# Exit 0 when every Python refusal routes through the seam; exit 1 naming file
# and line otherwise.
set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
TARGET="$REPO_ROOT/cli/src/fno/agents/spawn_gate.py"

if [[ ! -f "$TARGET" ]]; then
  echo "check-gate-refusals-emit: $TARGET not found" >&2
  exit 1
fi

# The one legal construction site: inside _refuse() itself.
LEGAL_LINE='    refusal = GateRefused(exit_code, receipt)'

fail=0
while IFS=: read -r lineno text; do
  [[ -z "${lineno:-}" ]] && continue
  if [[ "$text" == "$LEGAL_LINE" ]]; then
    continue
  fi
  if [[ $fail -eq 0 ]]; then
    echo "check-gate-refusals-emit: FAIL - a gate refusal bypasses the _refuse() emit seam." >&2
    echo "  Every refusal must exit through _refuse(), or it is invisible to the" >&2
    echo "  operator asking why a node did not launch. Offending line(s):" >&2
    fail=1
  fi
  echo "  cli/src/fno/agents/spawn_gate.py:${lineno}: ${text# }" >&2
done < <(grep -n 'GateRefused(' "$TARGET" | grep -v 'class GateRefused' | grep -v 'except GateRefused' | grep -v 'raises(spawn_gate.GateRefused' || true)

if [[ $fail -ne 0 ]]; then
  echo "" >&2
  echo "  Fix: call _refuse(<exit code>, <receipt or None>, **event) instead." >&2
  exit 1
fi

echo "check-gate-refusals-emit: ok (every GateRefused construction routes through _refuse; the transport is the only Python-side refusal source, see this script's header)"
