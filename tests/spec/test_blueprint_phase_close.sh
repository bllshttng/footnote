#!/usr/bin/env bash
set -euo pipefail

skill="skills/blueprint/SKILL.md"
gates="skills/blueprint/references/blueprint-gates.md"

grep -F 'fno backlog session close "$NODE_ID"' "$skill" >/dev/null
grep -F 'blueprint close readback: matched' "$skill" >/dev/null
grep -F 'Blueprint close refused:' "$skill" >/dev/null
grep -F 'Blueprint provenance is written by the identity-guarded `fno backlog session close` transaction' "$gates" >/dev/null
grep -F 'Plan binding is artifact-only' "$skill" >/dev/null

echo "blueprint phase close spec: positive close, readback, and refusal markers found"
