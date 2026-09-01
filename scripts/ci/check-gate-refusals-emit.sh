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
# an absent one. This covers the PYTHON gate only. `crates/fno-agents/src/
# spawn_gate.rs` is a second live gate (called from bin/client.rs for the
# daemon-client bg/headless arms) and it still refuses silently on the RAM
# floor, the load ceiling and max_live. It is not covered here, and the fix is
# blocked on a real prerequisite rather than on effort: Python's emit target is
# `paths.state_dir()/events.jsonl`, which is CONFIG-RESOLVED
# (cli/src/fno/paths.py:557), and Rust has no equivalent resolver. A Rust emit
# that guessed the path would write where nothing reads - a silent instrument,
# which is the failure this whole change exists to remove. Epic x-6f9f (path
# consolidation) owns that resolver.
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

echo "check-gate-refusals-emit: ok (every GateRefused construction routes through _refuse; the Rust gate is out of scope, see this script's header)"
