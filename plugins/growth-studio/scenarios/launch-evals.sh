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

# In this checkout the harness loads the repo-root copies, not the pack's: the
# claude plugin manifest reads skills/ and agents/ at the root. Nothing keeps
# the two copies in sync, so grading the pack copy alone lets a rename in the
# live skill pass here against a stale duplicate - the exact drift this file
# exists to catch. Grade the live copy and require the pair to be identical.
# A consuming project that installed only the pack has no root copy; there the
# pack copy IS the live one and these two lines are skipped.
roles=(growth-comms growth-designer growth-marketer growth-social)
agent_files=()
for role in "${roles[@]}"; do agent_files+=("$agents_dir/$role.md"); done

root_dir="$(cd "$pack_dir/../.." && pwd)"
live_skill="$root_dir/skills/growth-launch/SKILL.md"
if [[ -f "$live_skill" ]]; then
  sync_cond="diff -q '$live_skill' '$skill' >/dev/null"
  for role in "${roles[@]}"; do
    sync_cond+=" && diff -q '$root_dir/agents/$role.md' '$agents_dir/$role.md' >/dev/null"
  done
  check copy-sync \
    "the packaged skill and agents are identical to the ones the harness loads" \
    "$sync_cond"
  skill="$live_skill"
  agent_files=()
  for role in "${roles[@]}"; do agent_files+=("$root_dir/agents/$role.md"); done
fi

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
# no-dispatch used to be `! grep -E '^tools:.*(Bash|Task|...)' "$agents_dir"`,
# which is absence-shaped and cannot tell "no publish-capable tool" from "the
# instrument found nothing to read". grep exits 1 on a file with no `tools:`
# key at all and 2 on a missing directory, and both invert to PASS - so an
# agent whose tools: line was DELETED, which inherits every tool including
# Bash, certified as safe. That is the permission state this gate exists to
# forbid. Assert the positive marker instead: every role file is present, each
# declares tools:, and every tool it declares is on the allowlist.
allowed='Read|Write|Glob|Grep'
for agent_file in "${agent_files[@]}"; do
  role="$(basename "$agent_file" .md)"
  check "no-dispatch:$role" \
    "$role declares a tools: line and every tool on it is read/write-local" \
    "grep -qE '^tools:' '$agent_file' \
      && [ -z \"\$(grep -E '^tools:' '$agent_file' \
           | grep -oE '\"[A-Za-z_]+\"' | tr -d '\"' \
           | grep -vE '^($allowed)\$')\" ]"
done

exit $fail
