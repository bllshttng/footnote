#!/usr/bin/env bash
# migrate-state-dir.sh -- one-time operator migration of the global state dir
# from the old ~/.abilities to ~/.fno (rename: abilities -> fno).
#
# This is the AC3 migration step. It is SEPARATE from the code sweep
# (rename-to-fno.sh) and is run ONCE by the operator after installing the
# renamed `fno`. It is:
#   - idempotent: a no-op if ~/.fno already exists, or if ~/.abilities is absent
#     (a fresh install starts clean on ~/.fno -- AC3-EDGE)
#   - loud-failing: if the move cannot complete it exits non-zero and leaves the
#     legacy dir intact -- never a half-moved/lost state (AC3-FR)
#   - quiesce-aware: refuses to move while a live fno/abi worker holds a lock
#     under the legacy dir, so no in-flight write is lost mid-move
#
# A one-release READ-FALLBACK also exists in code (fno.paths.state_dir): until
# this migration is run, fno reads ~/.abilities and prints a one-time notice.
#
# Usage:
#   scripts/rename/migrate-state-dir.sh           # migrate ~/.abilities -> ~/.fno
#   FNO_HOME=/custom scripts/rename/migrate-state-dir.sh   # honor an override
#   scripts/rename/migrate-state-dir.sh --force   # skip the live-lock check
set -euo pipefail

FORCE=0
[[ "${1:-}" == "--force" ]] && FORCE=1

# Resolve the new + legacy dirs, honoring an explicit FNO_HOME override.
NEW="${FNO_HOME:-$HOME/.fno}"
LEGACY="$HOME/.abilities"

if [[ -e "$NEW" ]]; then
  # Footgun guard: the renamed `fno` auto-writes a FRESH-DEFAULT ~/.fno
  # (settings.yaml + .path-migration-done sentinel, no real state) on its first
  # invocation. If the real state still lives in the legacy dir, that stray
  # default would otherwise make us skip the migration and orphan the backlog.
  # Detect a stray (no graph.json/ledger.json in NEW) while LEGACY holds state,
  # set it aside non-destructively, and proceed with the real move.
  if [[ ! -e "$NEW/graph.json" && ! -e "$NEW/ledger.json" \
        && -d "$LEGACY" && ( -e "$LEGACY/graph.json" || -e "$LEGACY/ledger.json" ) ]]; then
    bak="$NEW.stray-default.bak"
    echo "migrate-state-dir: $NEW holds only fresh defaults (no graph/ledger);"
    echo "  the real state is in $LEGACY. Setting the stray aside at $bak and migrating."
    rm -rf "$bak"
    mv "$NEW" "$bak"
  else
    echo "migrate-state-dir: $NEW already exists with state -- nothing to do."
    exit 0
  fi
fi

if [[ ! -d "$LEGACY" ]]; then
  echo "migrate-state-dir: no legacy $LEGACY -- fresh install starts clean on $NEW."
  exit 0
fi

# Quiesce check: a live worker holding a lock under the legacy dir would lose an
# in-flight write if we move out from under it. The claims live in
# $LEGACY/claims/*.lock; treat any present claim as "busy" unless --force.
if [[ $FORCE -eq 0 && -d "$LEGACY/claims" ]]; then
  live="$(find "$LEGACY/claims" -name '*.lock' -type f 2>/dev/null | head -1 || true)"
  if [[ -n "$live" ]]; then
    cat >&2 <<EOF
migrate-state-dir: ERROR -- a claim lock is present under $LEGACY/claims.
A live fno/abi worker may be writing $LEGACY. Quiesce workers (let them finish,
or 'fno claim list' to inspect) and re-run. Use --force to override.
EOF
    exit 1
  fi
fi

# Atomic-ish move. mv within the same filesystem ($HOME) is atomic; if it
# straddles filesystems mv copies-then-deletes and a failure leaves the source
# intact, which is what we want (loud fail, no partial NEW).
echo "migrate-state-dir: moving $LEGACY -> $NEW ..."
if ! mv "$LEGACY" "$NEW"; then
  echo "migrate-state-dir: ERROR -- move failed; legacy $LEGACY left intact." >&2
  exit 1
fi

# Verify the move landed and the key state files survived.
missing=0
for f in graph.json ledger.json; do
  if [[ -e "$LEGACY/$f" ]]; then continue; fi   # was never present, fine
done
if [[ ! -d "$NEW" ]]; then
  echo "migrate-state-dir: ERROR -- $NEW missing after move; investigate." >&2
  exit 1
fi

echo "migrate-state-dir: done. State migrated to $NEW."
echo "  (graph/ledger/claims/briefs/fleet are byte-identical; only the parent path changed.)"
