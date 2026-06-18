#!/usr/bin/env bash
# CI gate: verify committed skill bundles match what the generator would
# produce from the current manifest + canonical sources. Fails with a clear
# message when someone forgot to regenerate.
#
# Compares all three bundle types (file, reference, agent) by re-running the
# generator into a tmp dir and cmp-ing each expected dest. Same logic for
# each type; the diff catches frontmatter drift on references/agents.
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

TMP="$(mktemp -d)"
ROWS_FILE="$(mktemp)"
trap 'rm -rf "$TMP" "$ROWS_FILE"' EXIT

# Generate into temp dir.
REPO_ROOT="$TMP" bash "$REPO_ROOT/scripts/generate-skill-bundles.sh" >/dev/null

# Capture parser output to a tempfile so we can check its rc; process
# substitution `done < <(...)` would discard a non-zero parser exit and
# silently report "fresh" on a malformed manifest.
if ! python3 "$REPO_ROOT/scripts/lib/parse-bundle-manifest.py" "$REPO_ROOT/skill-bundles.yaml" > "$ROWS_FILE"; then
  echo "ERROR: parse-bundle-manifest.py failed" >&2
  exit 2
fi

DRIFT=0
while IFS=$'\t' read -r TYPE SKILL SOURCE DEST META; do
  if [[ -z "$TYPE" ]]; then
    continue
  fi
  COMMITTED="$REPO_ROOT/skills/$SKILL/$DEST"
  GENERATED="$TMP/skills/$SKILL/$DEST"
  if [[ ! -f "$COMMITTED" ]]; then
    echo "ERROR: missing bundle: skills/$SKILL/$DEST (run scripts/generate-skill-bundles.sh)" >&2
    DRIFT=1
    continue
  fi
  if ! cmp -s "$COMMITTED" "$GENERATED"; then
    echo "ERROR: skills/$SKILL/$DEST out of sync with canonical $SOURCE [$TYPE]" >&2
    DRIFT=1
  fi
done < "$ROWS_FILE"

if [[ $DRIFT -ne 0 ]]; then
  echo "" >&2
  echo "Run scripts/generate-skill-bundles.sh and commit the result." >&2
  exit 1
fi
echo "skill bundles fresh"
