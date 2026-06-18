#!/usr/bin/env bash
# Block ${REPO_ROOT}/scripts/ references in any skills/*.md file.
# Skills must call ${SKILL_DIR}/scripts/X (preferred for portability) or
# `fno <verb>` (when the polished CLI surface is the right abstraction).
#
# Reason: ${REPO_ROOT}/scripts/X resolves only inside the abilities repo
# itself. In any other project where the abilities plugin is installed,
# the path is missing and skills silently no-op (or fail). Bundles live
# alongside the skill and ship with the plugin; CLI verbs resolve via PATH.
#
# See: skill-bundles.yaml, scripts/generate-skill-bundles.sh, and
# feedback memory feedback_skill_scripts_must_live_in_skill_bundle.md.
set -euo pipefail

# Resolve REPO_ROOT defensively. The naive $(git rev-parse ...) inside
# command substitution can propagate git's rc=128 silently when bash is
# running with inherit_errexit (seen on GitHub Actions ubuntu-latest with
# bash 5.x). The explicit `if ! ...; then` form contains the failure
# regardless of inherit_errexit semantics.
REPO_ROOT=""
if git_root=$(git rev-parse --show-toplevel 2>/dev/null); then
  REPO_ROOT="$git_root"
fi
if [[ -z "$REPO_ROOT" ]]; then
  SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  candidate="$SCRIPT_DIR"
  while [[ "$candidate" != "/" && "$candidate" != "." ]]; do
    if [[ -e "$candidate/.git" ]]; then
      REPO_ROOT="$candidate"
      break
    fi
    candidate="$(dirname "$candidate")"
  done
fi
if [[ -z "$REPO_ROOT" ]]; then
  echo "ERROR: not in a git repo (git rev-parse failed and no .git found via script-dir walk-up)" >&2
  exit 2
fi

# Match ${REPO_ROOT}/scripts/ and $REPO_ROOT/scripts/ (with or without
# braces). Skill .md files only - .py / .sh inside skills/<name>/scripts/
# legitimately reach into the bundled scripts and may use $REPO_ROOT for
# their own purposes (e.g., to write into .fno at the project root).
HITS="$(grep -rEn '\$\{?REPO_ROOT\}?/scripts/' "$REPO_ROOT/skills/" --include='*.md' 2>/dev/null || true)"

if [[ -n "$HITS" ]]; then
  echo "ERROR: skills must not reference \${REPO_ROOT}/scripts/." >&2
  echo "Use \${SKILL_DIR}/scripts/ (preferred) or fno CLI verbs." >&2
  echo "" >&2
  echo "$HITS" >&2
  exit 1
fi
echo "no \${REPO_ROOT}/scripts/ references in skills/"
