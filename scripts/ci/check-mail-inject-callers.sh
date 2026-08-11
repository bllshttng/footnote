#!/usr/bin/env bash
# check-mail-inject-callers.sh - ratchet on every site that builds a mail-inject argv.
#
# Forgetting is refused at runtime. Adding is refused at CI. Neither one passes
# forever by describing today.
#
# The Rust `mail-inject` binary is the second unwrapped door onto the transport.
# A new Python site that pipes unframed prose through it would bypass the
# command-only predicate's premise (every unframed payload is a slash command).
# This gate fails CI the moment such a site appears, until someone records it in
# the baseline with a reason. Keyed on file::function, not file:line, so a
# reorder inside an existing caller does not trip the ratchet; a NEW caller does.
#
# Run: bash scripts/ci/check-mail-inject-callers.sh
# Exit: 0 the caller set matches the baseline, 1 a new or removed site, 2 misuse.

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
BASELINE="$REPO_ROOT/scripts/ci/mail-inject-callers-baseline.txt"

if [[ ! -f "$BASELINE" ]]; then
  echo "check-mail-inject-callers: baseline missing at $BASELINE" >&2
  exit 2
fi

FOUND="$(mktemp)"
BASE="$(mktemp)"
trap 'rm -f "$FOUND" "$BASE"' EXIT

# Enumerate every `mail-inject` argv token in cli/src, resolved to its enclosing
# function. A new function building this argv is the signal this gate catches.
python3 - "$REPO_ROOT" <<'PY' | sort -u > "$FOUND"
import re
import sys
from pathlib import Path

root = Path(sys.argv[1])
out = []
for path in sorted((root / "cli" / "src").rglob("*.py")):
    rel = path.relative_to(root).as_posix()
    fn = ""
    for line in path.read_text(encoding="utf-8").splitlines():
        m = re.match(r"^[ \t]*def ([A-Za-z_]\w*)\b", line)
        if m:
            fn = m.group(1)
        if '"mail-inject"' in line or "'mail-inject'" in line:
            out.append(f"{rel}::{fn}")
print("\n".join(out))
PY

# Baseline keys: the first token of each non-comment, non-blank line.
grep -vE '^[[:space:]]*(#|$)' "$BASELINE" | awk '{print $1}' | sort -u > "$BASE"

ADDED="$(comm -23 "$FOUND" "$BASE" || true)"
REMOVED="$(comm -13 "$FOUND" "$BASE" || true)"

if [[ -z "$ADDED" && -z "$REMOVED" ]]; then
  echo "check-mail-inject-callers: ok ($(wc -l < "$BASE" | tr -d ' ') site(s) match the baseline)"
  exit 0
fi

if [[ -n "$ADDED" ]]; then
  echo "check-mail-inject-callers: NEW mail-inject argv site(s) not in the baseline:" >&2
  printf '  %s\n' $ADDED >&2
  echo "  Add each to scripts/ci/mail-inject-callers-baseline.txt with a reason the" >&2
  echo "  payload is framed or a slash command, or route the prose through fno mail send." >&2
fi
if [[ -n "$REMOVED" ]]; then
  echo "check-mail-inject-callers: baseline lists site(s) no longer present:" >&2
  printf '  %s\n' $REMOVED >&2
  echo "  Remove the stale line(s) from the baseline." >&2
fi
exit 1
