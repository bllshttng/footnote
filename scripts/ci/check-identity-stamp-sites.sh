#!/usr/bin/env bash
# check-identity-stamp-sites.sh - ratchet on every caller of the raw identity
# precedence primitive.
#
# `resolve_harness_identity` returns the first ambient harness marker in
# precedence order. That is correct when exactly one harness family is present
# and wrong the moment a process inherits a foreign one, so its own docstring
# says callers that STAMP identity onto a durable record must use
# `resolve_owned_identity` instead. That sentence was a documented rule with
# nothing enforcing it, and the caller set drifted from two obeying it to
# dozens not.
#
# This gate is the enforcement. Every remaining caller of the precedence
# primitive is recorded in the baseline with the reason it is read-only. A NEW
# caller fails CI until someone either routes it through
# `fno.claims.self_identity.resolve_self_identity` (a durable stamp) or records it here
# with a reason (a read or a branch).
#
# Keyed on file::function, not file:line, so a reorder inside an existing caller
# does not trip the ratchet; a NEW caller does. Failures still report the line,
# because that is what the reader needs to go look at.
#
# Known limit, stated rather than implied: the scan matches the name literally,
# so an aliased import (`import resolve_harness_identity as rhi`) is invisible
# to it. Nothing in the tree does that today, and a rename would be a
# deliberate act to dodge this gate rather than an accident.
#
# Run: bash scripts/ci/check-identity-stamp-sites.sh
# Exit: 0 the caller set matches the baseline, 1 a new or removed site, 2 misuse.

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
BASELINE="$REPO_ROOT/scripts/ci/identity-stamp-sites-baseline.txt"

if [[ ! -f "$BASELINE" ]]; then
  echo "check-identity-stamp-sites: baseline missing at $BASELINE" >&2
  exit 2
fi

FOUND="$(mktemp)"
KEYS="$(mktemp)"
BASE="$(mktemp)"
trap 'rm -f "$FOUND" "$KEYS" "$BASE"' EXIT

# Enumerate every `resolve_harness_identity(` call in cli/src, resolved to its
# enclosing function. Tests are excluded: they exercise the primitive on purpose
# and stamp nothing durable. `harness_identity.py` itself is excluded because it
# DEFINES the function; a self-reference is not a caller.
python3 - "$REPO_ROOT" <<'PY' | sort -u > "$FOUND"
import re
import sys
from pathlib import Path

root = Path(sys.argv[1])
out = []
for path in sorted((root / "cli" / "src").rglob("*.py")):
    rel = path.relative_to(root).as_posix()
    if path.name == "harness_identity.py" or path.name.startswith("test_"):
        continue
    fn = ""
    fn_indent = None
    for lineno, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        # A class body opens a fresh def scope, so its methods are attributable.
        if re.match(r"^class [A-Za-z_]", line):
            fn, fn_indent = "", None
            continue
        # Outermost defs only: a nested helper closure is indented deeper than
        # its enclosing def and must not steal that def's attribution.
        m = re.match(r"^([ \t]*)def ([A-Za-z_]\w*)\b", line)
        if m and (fn_indent is None or len(m.group(1)) <= fn_indent):
            fn, fn_indent = m.group(2), len(m.group(1))
        # The CALL, not the import line and not a mention in prose.
        if re.search(r"\bresolve_harness_identity\s*\(", line) and not line.lstrip().startswith(
            ("#", "from ", "import ")
        ):
            out.append(f"{rel}::{fn} {rel}:{lineno}")
print("\n".join(out))
PY

awk '{print $1}' "$FOUND" | sort -u > "$KEYS"

# Baseline keys: the first token of each non-comment, non-blank line.
grep -vE '^[[:space:]]*(#|$)' "$BASELINE" | awk '{print $1}' | sort -u > "$BASE"

CLASSIFIED="$(wc -l < "$KEYS" | tr -d ' ')"

# Positive marker, not an absence (AC5-HP). A regex that matched nothing would
# otherwise report a clean caller set and pass forever - the failure mode where
# a gate reads as protection while measuring nothing.
if [[ "$CLASSIFIED" -eq 0 ]]; then
  echo "check-identity-stamp-sites: classified 0 sites - the scan found nothing." >&2
  echo "  The primitive has callers; a zero here means the enumerator is broken," >&2
  echo "  not that the tree is clean. Fix the scan before trusting this gate." >&2
  exit 2
fi

ADDED="$(comm -23 "$KEYS" "$BASE" || true)"
REMOVED="$(comm -13 "$KEYS" "$BASE" || true)"

if [[ -z "$ADDED" && -z "$REMOVED" ]]; then
  echo "check-identity-stamp-sites: ok (classified $CLASSIFIED read-only site(s), all in the baseline)"
  exit 0
fi

if [[ -n "$ADDED" ]]; then
  echo "check-identity-stamp-sites: NEW resolve_harness_identity caller(s) not in the baseline:" >&2
  for key in $ADDED; do
    # Report the line, which is what the reader needs to go look at.
    printf '  %s\n' "$(grep -m1 -F "$key " "$FOUND" | awk '{print $2}') ($key)" >&2
  done
  echo "  If the resolved harness or session id is WRITTEN to a claim, a mail record," >&2
  echo "  an event, an agent-state row, a registry row, a crown grant, a decision" >&2
  echo "  record or a graph session record, use fno.claims.self_identity" >&2
  echo "  instead - the precedence primitive launders an inherited marker into" >&2
  echo "  ownership. If it only reads to display or branch, add it to" >&2
  echo "  scripts/ci/identity-stamp-sites-baseline.txt with that reason." >&2
fi
if [[ -n "$REMOVED" ]]; then
  echo "check-identity-stamp-sites: baseline lists site(s) no longer present:" >&2
  printf '  %s\n' $REMOVED >&2
  echo "  Remove the stale line(s) from the baseline." >&2
fi
exit 1
