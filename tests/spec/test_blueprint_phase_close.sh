#!/usr/bin/env bash
set -euo pipefail

skill="skills/blueprint/SKILL.md"
gates="skills/blueprint/references/blueprint-gates.md"

# The fail-open plan-bind stamp writer was removed; its reintroduction as
# blueprint guidance would regress blueprint close to a skipped provenance note.
# Guarding absence of the retired implementation, not the presence of doc prose.
if grep -F '_stamp_blueprint_on_plan_link' "$skill" "$gates" >/dev/null; then
  echo "blueprint phase close spec: obsolete plan-bind blueprint writer reintroduced" >&2
  exit 1
fi

echo "blueprint phase close spec: retired plan-bind writer still absent"
