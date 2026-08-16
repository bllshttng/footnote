#!/usr/bin/env bash
# check-autonomy-registry.sh - ratchet on every spawn-shaped call site, Python
# and shell, so a tenth autonomous door cannot be added silently (x-aaaf wave
# 3.3, AC6-CON).
#
# "A guard placed on one of N reachable paths is decorative" is the governing
# risk here: nine-plus spawners span cli/src (Python) and skills/*/scripts +
# hooks (shell). A Python-only lint would miss every shell caller. This gate
# enumerates both and fails CI the moment a new site appears, until someone
# records it in the baseline naming which `fno autonomy status` spawner it
# belongs to, or an explicit reason it is not autonomous.
#
# Run: bash scripts/ci/check-autonomy-registry.sh
# Exit: 0 the site set matches the baseline, 1 a new or removed site, 2 misuse.

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
BASELINE="$REPO_ROOT/scripts/ci/autonomy-registry-baseline.txt"

if [[ ! -f "$BASELINE" ]]; then
  echo "check-autonomy-registry: baseline missing at $BASELINE" >&2
  exit 2
fi

FOUND="$(mktemp)"
BASE="$(mktemp)"
trap 'rm -f "$FOUND" "$BASE"' EXIT

# Python: a spawn-shaped call site builds an `"agents", "spawn"` argv, often
# spread one item per line, so the match is a windowed "spawn" within a few
# lines of "agents" rather than a single-line regex. Resolved to its
# outermost enclosing function, like check-mail-inject-callers.sh.
python3 - "$REPO_ROOT" <<'PY' | sort -u > "$FOUND"
import re
import sys
from pathlib import Path

root = Path(sys.argv[1])
out = []
for path in sorted((root / "cli" / "src").rglob("*.py")):
    rel = path.relative_to(root).as_posix()
    fn = ""
    fn_indent = None
    lines = path.read_text(encoding="utf-8").splitlines()
    for i, line in enumerate(lines):
        if re.match(r"^class [A-Za-z_]", line):
            fn, fn_indent = "", None
            continue
        m = re.match(r"^([ \t]*)def ([A-Za-z_]\w*)\b", line)
        if m and (fn_indent is None or len(m.group(1)) <= fn_indent):
            fn, fn_indent = m.group(2), len(m.group(1))
        if re.search(r'["\']agents["\']', line):
            window = "\n".join(lines[i:i + 4])
            if re.search(r'["\']spawn["\']', window):
                out.append(f"{rel}::{fn}")
print("\n".join(out))
PY

# Shell: file-level (bash has no reliable function-boundary AST). Scoped to
# the surface the design doc names - skills/*/scripts, hooks/, scripts/ -
# excluding test fixtures, which exercise the real scripts rather than being
# reachable themselves.
python3 - "$REPO_ROOT" <<'PY' >> "$FOUND"
import sys
from pathlib import Path

root = Path(sys.argv[1])
dirs = [root / "skills", root / "hooks", root / "scripts"]
out = []
for d in dirs:
    if not d.is_dir():
        continue
    for path in sorted(d.rglob("*.sh")):
        rel = path.relative_to(root).as_posix()
        if "/tests/" in rel or "/test/" in rel:
            continue
        if rel == "scripts/ci/check-autonomy-registry.sh":
            continue  # self-referential: this script's own text names the marker
        if "fno agents spawn" in path.read_text(encoding="utf-8"):
            out.append(f"{rel}::")
print("\n".join(out))
PY

sort -u -o "$FOUND" "$FOUND"

# Baseline keys: the first token of each non-comment, non-blank line.
grep -vE '^[[:space:]]*(#|$)' "$BASELINE" | awk '{print $1}' | sort -u > "$BASE"

ADDED="$(comm -23 "$FOUND" "$BASE" || true)"
REMOVED="$(comm -13 "$FOUND" "$BASE" || true)"

if [[ -z "$ADDED" && -z "$REMOVED" ]]; then
  echo "check-autonomy-registry: ok ($(wc -l < "$BASE" | tr -d ' ') site(s) match the baseline)"
  exit 0
fi

if [[ -n "$ADDED" ]]; then
  echo "check-autonomy-registry: NEW spawn-shaped call site(s) not in the baseline:" >&2
  printf '  %s\n' $ADDED >&2
  echo "  Add each to scripts/ci/autonomy-registry-baseline.txt naming the" >&2
  echo "  fno autonomy status spawner it belongs to, add a status row via" >&2
  echo "  cli/src/fno/autonomy_cli.py if it is a genuinely new spawner, or" >&2
  echo "  record why it is not autonomous (an explicit operator command)." >&2
fi
if [[ -n "$REMOVED" ]]; then
  echo "check-autonomy-registry: baseline lists site(s) no longer present:" >&2
  printf '  %s\n' $REMOVED >&2
  echo "  Remove the stale line(s) from the baseline." >&2
fi
exit 1
