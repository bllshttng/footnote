#!/usr/bin/env bash
# corrections-migrate-to-fno.sh - one-time migration of corrections.log and
# corrections-rejected.log from the legacy ~/.claude/ location to ~/.fno/
# (placement rule, ab-f063 Wave 2: footnote state never lives under .claude/).
#
# Idempotent: a migrated old file is left as a one-line tombstone, so a
# repeated run (or an old writer still targeting the legacy path) is a no-op.
# Safe order per file: append old content to the new location, verify the
# line count landed, THEN tombstone the old path. Never deletes the old file
# outright - the tombstone is the record that migration happened; delete it
# by hand once you've confirmed the new location is working (next release).
#
# Usage: scripts/corrections-migrate-to-fno.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/corrections-lock.sh
source "$SCRIPT_DIR/lib/corrections-lock.sh"

_migrate_one() {
  local old="$1" new="$2" label="$3"

  if [[ ! -f "$old" ]]; then
    echo "corrections-migrate: no legacy $label at $old; nothing to do" >&2
    return 0
  fi
  # A tombstone is a single "# migrated to ..." line - already done.
  if head -1 "$old" 2>/dev/null | grep -q '^# migrated to '; then
    echo "corrections-migrate: $old already tombstoned; skipping" >&2
    return 0
  fi

  mkdir -p "$(dirname "$new")"
  local lock_dir="${new}.lock.d"
  if ! _corrections_acquire_lock "$lock_dir" 5; then
    echo "corrections-migrate: could not acquire lock on $new after 5s; aborting" >&2
    return 1
  fi

  local old_lines before_lines after_lines
  old_lines=$(wc -l < "$old" | tr -d ' ')
  before_lines=$( (wc -l < "$new" 2>/dev/null || echo 0) | tr -d ' ')
  cat "$old" >> "$new"
  # corrections-log-init.sh's mode-0600 invariant applies here too - a fresh
  # $new created by this append otherwise lands at the umask default (codex
  # review, PR #185).
  chmod 600 "$new" 2>/dev/null || true
  after_lines=$(wc -l < "$new" | tr -d ' ')
  _corrections_release_lock "$lock_dir"

  local expected=$((before_lines + old_lines))
  if [[ "$after_lines" -ne "$expected" ]]; then
    echo "corrections-migrate: verification FAILED for $label: $new has $after_lines line(s), expected $expected. NOT tombstoning $old - investigate before re-running." >&2
    return 1
  fi

  printf '# migrated to %s on %s - see git history for prior content\n' \
    "$new" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$old"
  echo "corrections-migrate: moved $old_lines line(s) from $old to $new; tombstoned $old" >&2
}

CLAUDE_DIR="${CLAUDE_DIR_OVERRIDE:-$HOME/.claude}"
_migrate_one "$CLAUDE_DIR/corrections.log" "$(corrections_log_path)" "corrections.log"
_migrate_one "$CLAUDE_DIR/corrections-rejected.log" "$(corrections_rejected_log_path)" "corrections-rejected.log"
