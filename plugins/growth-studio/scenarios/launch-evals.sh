#!/usr/bin/env bash
# launch-evals.sh - behavior assertions for the growth-launch skill.
#
# Each assertion is reported pass or fail individually; the first failure exits
# non-zero. The orchestrator is a prose skill, so its behavior contract is
# verified as a present, consistent claim against SKILL.md and the pack (this
# mirrors the marketingskills evals shape: prompt, expected_output, assertions).
# Honesty rule: recorded_result in the manifest reflects this real run.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
pack_dir="$(cd "$script_dir/.." && pwd)"
skill="$pack_dir/skills/growth-launch/SKILL.md"
agents_dir="$pack_dir/agents"

fail=0
check() {
  local id="$1" claim="$2" cond="$3"
  if eval "$cond"; then
    echo "PASS $id: $claim"
  else
    echo "FAIL $id: $claim" >&2
    fail=1
  fi
}

[[ -f "$skill" ]] || { echo "FAIL activation-gate: missing skill at $skill" >&2; exit 1; }

check activation-gate \
  "refuses with the exact activate line when inactive" \
  "grep -qF 'fno config plugins activate plugins/growth-studio/plugin.yaml' '$skill'"
check resolution-gate \
  "resolves each role (capability/context gates) before dispatch" \
  "grep -qiE 'resolve each role|fno agents roles resolve' '$skill'"
check one-draft-round \
  "exactly one draft round runs" \
  "grep -qiE 'one draft round' '$skill'"
check one-revision-round \
  "at most one revision round runs; a second failure is not retried" \
  "grep -qiE 'at most one revision round' '$skill'"
check evidence-verdicts \
  "every draft carries a factual and a brand verdict file" \
  "grep -qi 'factual' '$skill' && grep -qi 'brand' '$skill'"
check terminal-state \
  "terminal state is approved-draft-bundle" \
  "grep -qiF 'approved-draft-bundle' '$skill'"
check no-dispatch \
  "no packaged agent holds a publish-capable tool" \
  "! grep -rhE '^tools:.*(Bash|Task|WebSearch|WebFetch|mcp__)' '$agents_dir' 2>/dev/null"

exit $fail
