#!/usr/bin/env bash
# Marketplace-readiness lint for driver skills.
#
# Verifies that each driver skill (/target, /megawalk, /megatron) is
# self-contained at the skill-folder level so the folder can be lifted
# into any markdown-aware runtime (Codex, Gemini, openclaw, etc.) without
# the surrounding abilities-plugin context.
#
# Three checks per driver skill:
#   1. No Skill() runtime calls. Driver skills must use Read for
#      progressive disclosure and Task/Agent for dispatched subagents,
#      never the Skill tool at runtime.
#   2. No ../../_shared/ or ../../<sibling-skill>/ path escapes. Cross-skill
#      content reuse must happen at BUILD TIME via skill-bundles.yaml, so
#      each skill folder is self-contained at checkout.
#   3. The skill's SKILL.md declares its `fno` binary dependency under
#      `requires.binaries` in frontmatter. Mirrors how an npm skill
#      declares `node` or a CLI tool declares `gh`.
#
# Usage:
#   bash scripts/lint/no-cross-skill-runtime-calls.sh
#   bash scripts/lint/no-cross-skill-runtime-calls.sh --root /tmp/fixture
#
# The --root flag lets tests point the lint at a fixture tree instead
# of the real repo. Default: $(git rev-parse --show-toplevel).
set -euo pipefail

# ---------------------------------------------------------------------------
# Arg parsing
# ---------------------------------------------------------------------------
ROOT=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --root)
      ROOT="$2"
      shift 2
      ;;
    --root=*)
      ROOT="${1#--root=}"
      shift
      ;;
    -h|--help)
      sed -n '2,/^# Usage/p' "$0" >&2
      exit 0
      ;;
    *)
      echo "ERROR: unknown arg: $1" >&2
      exit 2
      ;;
  esac
done

if [[ -z "$ROOT" ]]; then
  ROOT="$(git rev-parse --show-toplevel 2>/dev/null || true)"
  if [[ -z "$ROOT" ]]; then
    echo "ERROR: not in a git repo and --root not set" >&2
    exit 2
  fi
fi

if [[ ! -d "$ROOT" ]]; then
  echo "ERROR: --root not a directory: $ROOT" >&2
  exit 2
fi

# Helper paths resolve from the lint script's own location so test
# fixtures (which set --root to a tmp dir that doesn't bundle helpers)
# still find the helpers in the real repo.
SCRIPT_DIR_REAL="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT_REPO_ROOT="$(cd "$SCRIPT_DIR_REAL/../.." && pwd)"
FRONTMATTER_CHECKER="$SCRIPT_REPO_ROOT/scripts/lib/check-skill-frontmatter.py"

# ---------------------------------------------------------------------------
# Driver skill list. Fixtures pass --root pointing at a tree where these
# subdirs exist. Missing dirs are tolerated (lint just skips them)
# so tests can fixture only the skills they care about.
#
#   DRIVER_SKILLS   - the pipeline drivers (target, megawalk, megatron).
#   CLUSTER_ROUTERS - the router+mode skills (epic ab-0d05a9b7). A router
#                     folds sibling skills in as modes via bundled references,
#                     so it must obey the same self-containment invariants:
#                     no Skill() runtime calls, no ../../ path escapes, and an
#                     `fno` binary declaration in frontmatter.
# ---------------------------------------------------------------------------
DRIVER_SKILLS=(target megawalk megatron)
CLUSTER_ROUTERS=(review fix think pr do)
SELF_CONTAINED_SKILLS=("${DRIVER_SKILLS[@]}" "${CLUSTER_ROUTERS[@]}")
EXIT=0

for skill in "${SELF_CONTAINED_SKILLS[@]}"; do
  SKILL_DIR="$ROOT/skills/$skill"
  if [[ ! -d "$SKILL_DIR" ]]; then
    continue
  fi

  # -------------------------------------------------------------------
  # Check 1: No Skill() runtime calls in any .md file in the skill folder.
  # -------------------------------------------------------------------
  HITS=$(grep -rnE 'Skill\(' "$SKILL_DIR" --include='*.md' 2>/dev/null || true)
  if [[ -n "$HITS" ]]; then
    echo "ERROR: Skill() runtime call in skills/$skill (forbidden in driver skills)." >&2
    echo "  Use Read for in-context progressive disclosure or Task/Agent for dispatched subagents." >&2
    echo "$HITS" >&2
    echo "" >&2
    EXIT=1
  fi

  # -------------------------------------------------------------------
  # Check 2: No ../../_shared/ or ../../<sibling-skill>/ path escapes.
  # Match both Markdown link-target form (X.md) and bare references.
  # -------------------------------------------------------------------
  HITS=$(grep -rnE '\.\./\.\./_shared/|\.\./\.\./[a-z][a-z0-9_-]*/' "$SKILL_DIR" --include='*.md' 2>/dev/null || true)
  if [[ -n "$HITS" ]]; then
    echo "ERROR: cross-skill path escape in skills/$skill (forbidden)." >&2
    echo "  Use bundled references/ or agents/ instead (see skill-bundles.yaml)." >&2
    echo "$HITS" >&2
    echo "" >&2
    EXIT=1
  fi

  # -------------------------------------------------------------------
  # Check 3: requires.binaries.fno declared in SKILL.md frontmatter.
  # Tolerates missing SKILL.md only when the directory is also missing
  # (handled by the outer `continue` above); past that, SKILL.md is
  # required and must declare the dep.
  # -------------------------------------------------------------------
  SKILL_FILE="$SKILL_DIR/SKILL.md"
  if [[ ! -f "$SKILL_FILE" ]]; then
    echo "ERROR: skills/$skill/SKILL.md missing." >&2
    EXIT=1
    continue
  fi
  # Frontmatter validation lives in scripts/lib/check-skill-frontmatter.py
  # so it can be tested in isolation and reused. The helper splits failure
  # modes: rc=2 for PyYAML-missing / file-missing / malformed-frontmatter
  # (substrate problems) and rc=1 for "binary not declared" (authoring
  # problem). The `if ...; then : ; else rc=$?; fi` pattern is tested
  # context, so `set -e` does NOT abort when the helper exits non-zero.
  rc=0
  if python3 "$FRONTMATTER_CHECKER" "$SKILL_FILE" --require fno; then
    :  # frontmatter OK and declares fno
  else
    rc=$?
  fi
  if [[ $rc -eq 2 ]]; then
    # PyYAML missing: the python helper already printed the install hint
    # to stderr. Emit a context-bearing follow-up so the user knows which
    # SKILL.md the lint was inspecting when the dep was missing.
    echo "  (while checking skills/$skill/SKILL.md frontmatter)" >&2
    echo "" >&2
    EXIT=1
  elif [[ $rc -ne 0 ]]; then
    echo "ERROR: skills/$skill/SKILL.md does not declare 'fno' under requires.binaries in frontmatter." >&2
    echo "  Add a requires: block to the frontmatter (see Phase 6 of the encapsulation refactor)." >&2
    echo "" >&2
    EXIT=1
  fi
done

if [[ $EXIT -eq 0 ]]; then
  echo "marketplace-readiness lint: all driver skills + cluster routers self-contained"
fi
exit $EXIT
