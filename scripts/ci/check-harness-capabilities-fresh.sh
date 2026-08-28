#!/usr/bin/env bash
# scripts/ci/check-harness-capabilities-fresh.sh
#
# Freshness gate: asserts cli/src/fno/agents/harness_capabilities.toml is byte-identical
# to the canonical source crates/fno-agents/src/harness_capabilities.toml.
#
# Fails CI when the generated copy diverges from the canonical source.
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
  # Fallback: walk up from script location looking for .git
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

CANONICAL_REL="crates/fno-agents/src/harness_capabilities.toml"
COPY_REL="cli/src/fno/agents/harness_capabilities.toml"

CANONICAL="$REPO_ROOT/$CANONICAL_REL"
COPY="$REPO_ROOT/$COPY_REL"

if [[ ! -f "$CANONICAL" ]]; then
  echo "ERROR: canonical harness_capabilities.toml missing at $CANONICAL_REL" >&2
  exit 2
fi

if [[ ! -f "$COPY" ]]; then
  echo "ERROR: copy harness_capabilities.toml missing at $COPY_REL" >&2
  exit 2
fi

if ! cmp -s "$CANONICAL" "$COPY"; then
  echo "ERROR: harness_capabilities.toml copies have diverged:" >&2
  echo "  canonical: $CANONICAL_REL" >&2
  echo "  copy:      $COPY_REL" >&2
  echo "" >&2
  echo "To resync, run:" >&2
  echo "  cp $CANONICAL_REL $COPY_REL" >&2
  exit 1
fi

echo "harness capabilities table fresh"
