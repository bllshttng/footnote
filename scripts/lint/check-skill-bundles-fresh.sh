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
  # Pack rows land at root-relative plugin paths (agents/<id>.md, skills/<id>);
  # the legacy types stay under skills/<skill>/.
  case "$TYPE" in
    pack-skill|pack-agent)
      COMMITTED="$REPO_ROOT/$DEST"
      GENERATED="$TMP/$DEST"
      ;;
    *)
      COMMITTED="$REPO_ROOT/skills/$SKILL/$DEST"
      GENERATED="$TMP/skills/$SKILL/$DEST"
      ;;
  esac
  if [[ "$TYPE" == "pack-skill" ]]; then
    # Directory bundle: compare the whole tree.
    if [[ ! -d "$COMMITTED" ]]; then
      echo "ERROR: missing pack skill bundle: $DEST (run scripts/generate-skill-bundles.sh)" >&2
      DRIFT=1
      continue
    fi
    if ! diff -rq "$COMMITTED" "$GENERATED" >/dev/null; then
      echo "ERROR: $DEST out of sync with canonical $SOURCE [$TYPE]" >&2
      DRIFT=1
    fi
    continue
  fi
  if [[ ! -f "$COMMITTED" ]]; then
    echo "ERROR: missing bundle: ${COMMITTED#$REPO_ROOT/} (run scripts/generate-skill-bundles.sh)" >&2
    DRIFT=1
    continue
  fi
  if ! cmp -s "$COMMITTED" "$GENERATED"; then
    echo "ERROR: ${COMMITTED#$REPO_ROOT/} out of sync with canonical $SOURCE [$TYPE]" >&2
    DRIFT=1
  fi
done < "$ROWS_FILE"

# Detect orphaned pack bundles: when a pack removes or renames a declared skill
# or agent, the parser emits no row for the old destination, so the loop above
# never examines it. Scan committed pack-marked outputs and flag any whose
# destination no pack currently declares - otherwise a stale agent stays
# callable while this gate reports fresh.
_pack_marker_value() {
  awk 'NR==1 && /^---[[:space:]]*$/ {f=1; next} f && /^---[[:space:]]*$/ {exit} f && /^pack:/ {sub(/^pack:[[:space:]]*/,""); gsub(/["'\'']/,""); print; exit}' "$1" 2>/dev/null
}
EXPECTED_PACK_DESTS="$(grep -E '^(pack-skill|pack-agent)' "$ROWS_FILE" | cut -f4 | sort -u)"
while IFS= read -r f; do
  [[ -f "$f" ]] || continue
  [[ -n "$(_pack_marker_value "$f")" ]] || continue
  dest="agents/$(basename "$f")"
  grep -qxF "$dest" <<<"$EXPECTED_PACK_DESTS" || { echo "ERROR: stale pack bundle $dest no longer declared by any pack; remove it" >&2; DRIFT=1; }
done < <(find "$REPO_ROOT/agents" -name '*.md' -type f 2>/dev/null)
while IFS= read -r d; do
  [[ -f "$d/SKILL.md" ]] || continue
  [[ -n "$(_pack_marker_value "$d/SKILL.md")" ]] || continue
  dest="skills/$(basename "$d")"
  grep -qxF "$dest" <<<"$EXPECTED_PACK_DESTS" || { echo "ERROR: stale pack bundle $dest no longer declared by any pack; remove it" >&2; DRIFT=1; }
done < <(find "$REPO_ROOT/skills" -mindepth 1 -maxdepth 1 -type d 2>/dev/null)

if [[ $DRIFT -ne 0 ]]; then
  echo "" >&2
  echo "Run scripts/generate-skill-bundles.sh and commit the result." >&2
  exit 1
fi
echo "skill bundles fresh"
