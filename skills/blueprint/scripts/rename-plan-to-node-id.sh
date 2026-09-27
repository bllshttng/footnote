#!/usr/bin/env bash
# rename-plan-to-node-id.sh (US5) - after a raw-prose /blueprint auto-intake
# mints a fresh node id for an id-LESS plan, give the artifact its node-bearing
# name and repoint the node's plan_path at it. Idempotent: a plan whose name
# already ends -<node-id>.md is a no-op, so a rerun never double-suffixes.
#
# Usage: rename-plan-to-node-id.sh <plan-path> <node-id>
# Output: exactly one status line, always. The two RESULT lines go to STDOUT
# (the caller reads them to learn the final path): `renamed <new-path>` |
# `already-node-bearing <path>`. Every SKIP/warn diagnostic goes to STDERR so a
# caller grepping stdout for the path is never confused by a non-path line:
# `skipped reason=<...>`. Never fatal to /blueprint (all paths exit 0).
set -uo pipefail

PLAN="${1:-}"
NODE="${2:-}"
if [[ -z "$PLAN" || -z "$NODE" ]]; then
  echo "skipped reason=missing-args" >&2
  exit 0
fi
if [[ ! -f "$PLAN" ]]; then
  echo "skipped reason=plan-not-found" >&2
  exit 0
fi

dir="$(dirname "$PLAN")"
base="$(basename "$PLAN")"

# Already node-bearing (this node) -> nothing to do. Guards the rerun case and
# the node-seeded paths (US1/US2/US3) that arrive named correctly.
if [[ "$base" == *"-${NODE}.md" ]]; then
  echo "already-node-bearing $PLAN"
  exit 0
fi

stem="${base%.md}"
new="$dir/${stem}-${NODE}.md"

# A pre-existing target is another node's/this node's doc; do not clobber it.
if [[ -e "$new" && "$new" != "$PLAN" ]]; then
  echo "skipped reason=target-exists path=$new" >&2
  exit 0
fi

if ! mv "$PLAN" "$new" 2>/dev/null; then
  echo "skipped reason=rename-failed" >&2
  exit 0
fi

# Key the frontmatter with the minted id when the plan names none. The
# filename alone encodes the id only for humans; the validator's node-id
# gates and the graph readers read the frontmatter, and a name-without-key
# plan fails the Plan Node Binding check on every later validation. The
# probe reads the frontmatter block only: a body line starting `node:` is
# prose, not a key.
fm_key=$(awk '/^---[[:space:]]*$/{c++; next} c<2 && /^(node|claims):/{print "y"; exit}' "$new")
if [[ -z "$fm_key" ]]; then
  tmp="$new.tmp"
  if awk -v id="$NODE" '
      /^---[[:space:]]*$/ && !done { print; print "node: " id; done=1; next }
      { print }
    ' "$new" > "$tmp" 2>/dev/null && mv "$tmp" "$new"; then
    :
  else
    rm -f "$tmp"
    echo "warn: frontmatter key write failed for $NODE (file renamed to $new)" >&2
  fi
fi

# Repoint the node. Non-fatal: the file already moved, and a stale plan_path is
# recoverable, so a CLI hiccup must not fail the blueprint handoff.
if command -v fno >/dev/null 2>&1; then
  fno backlog update "$NODE" --plan-path "$new" >/dev/null 2>&1 \
    || echo "warn: plan_path update failed for $NODE (file renamed to $new)" >&2
fi

echo "renamed $new"
