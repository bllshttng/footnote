#!/usr/bin/env bash
# check-opencode-surface.sh - CI gate over footnote's OpenCode discovery
# surface. OpenCode scans .opencode/skills/ (plus .claude/.agents), never the
# repo-root skills/ dir, so a skill shipped without a relative symlink in
# .opencode/skills/ is invisible to OpenCode on a fresh clone - the farm only
# existed as machine-local links until this guard's PR. The five advertised
# verbs need .opencode/commands/<verb>.md or there is no slash surface, and the
# stop-hook bridge's "/target --resume" re-drive text has nothing to resolve
# against.
#   farm      every skills/<name> valid under OpenCode's name regex has an
#             .opencode/skills/<name> symlink -> ../../skills/<name> that
#             resolves to a dir containing SKILL.md; no unbacked extras;
#             a skill dir OpenCode cannot name is a defect, not a skip
#   commands  the five advertised verbs exist as non-empty command files
# Run: bash scripts/ci/check-opencode-surface.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
FARM="$ROOT/.opencode/skills"
COMMANDS="$ROOT/.opencode/commands"
VERBS="target think review fix pr"

fail() {
  echo "FAIL: check-opencode-surface.sh: $*" >&2
  exit 1
}

# ---- farm freshness ---------------------------------------------------------

[[ -d "$FARM" ]] || fail ".opencode/skills/ missing - OpenCode discovers zero footnote skills"

declare -a expected=()
for entry in "$ROOT"/skills/*/; do
  name="$(basename "$entry")"
  [[ "$name" =~ ^[a-z0-9]+(-[a-z0-9]+)*$ ]] \
    || fail "skills/$name: name invalid for OpenCode (must match ^[a-z0-9]+(-[a-z0-9]+)*$); rename it or it is undiscoverable"
  expected+=("$name")
done
[[ ${#expected[@]} -gt 0 ]] || fail "skills/ has no skill dirs - nothing to expose (stale guard?)"

for name in "${expected[@]}"; do
  link="$FARM/$name"
  [[ -L "$link" ]] || fail ".opencode/skills/$name missing (skill invisible to OpenCode). Fix: ln -s ../../skills/$name .opencode/skills/$name"
  [[ "$(readlink "$link")" == "../../skills/$name" ]] \
    || fail ".opencode/skills/$name -> $(readlink "$link"): must be the relative ../../skills/$name so it resolves inside any checkout/worktree"
  [[ -f "$link/SKILL.md" ]] || fail ".opencode/skills/$name resolves but SKILL.md is not readable through the link"
done

for link in "$FARM"/*; do
  name="$(basename "$link")"
  [[ -d "$ROOT/skills/$name" ]] || fail ".opencode/skills/$name has no skills/$name backing (stale link)"
done

# ---- command surface --------------------------------------------------------

for verb in $VERBS; do
  file="$COMMANDS/$verb.md"
  [[ -s "$file" ]] || fail ".opencode/commands/$verb.md missing or empty (no /$verb surface; the bridge re-drive resolves /target)"
done

echo "PASS: opencode surface fresh (${#expected[@]} skill links, $(echo $VERBS | wc -w | tr -d ' ') commands)"
