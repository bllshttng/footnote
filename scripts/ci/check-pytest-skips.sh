#!/usr/bin/env bash
# check-pytest-skips.sh - ratchet on every pytest skip site in tracked .py files.
#
# A failing test blocks a merge; a skipped one reads green and its prose reason
# is read by nobody, so the cheapest way past a red test is to skip it. This
# gate fails CI the moment a skip site appears that the baseline does not name,
# until the PR that adds it records a line with a reason, or makes the test
# pass. Keyed on path::scope::kind, not line, so moving code inside a test
# does not trip the ratchet; a NEW site does.
#
# Run: bash scripts/ci/check-pytest-skips.sh
# Exit: 0 the site set matches the baseline, 1 a new or removed site (or a
#       baseline line without a reason), 2 misuse (baseline missing).

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
BASELINE="$REPO_ROOT/scripts/ci/pytest-skips-baseline.txt"

if [[ ! -f "$BASELINE" ]]; then
  echo "check-pytest-skips: baseline missing at $BASELINE" >&2
  exit 2
fi

FOUND="$(mktemp)"
BASE="$(mktemp)"
FILES="$(mktemp)"
trap 'rm -f "$FOUND" "$BASE" "$FILES"' EXIT

# A baseline key without a reason is the prose-nobody-reads failure this gate
# closes, so the file itself is held to the same rule it enforces.
awk '
  NF == 0 || $1 ~ /^#/ { next }
  {
    idx = index($0, "#"); reason = ""
    if (idx > 0) reason = substr($0, idx + 1)
    gsub(/^[ \t\r]+|[ \t\r]+$/, "", reason)
    if (reason == "") {
      printf "check-pytest-skips: baseline line %d has no reason: %s\n", NR, $1
      bad = 1
    }
  }
  END { exit bad ? 1 : 0 }
' "$BASELINE" >&2

# Baseline keys: the first token of each non-comment, non-blank line. Order
# and duplicates kept - comm compares multisets, so a second skip in a test
# that already has one is a new line.
grep -vE '^[[:space:]]*(#|$)' "$BASELINE" | awk '{print $1}' | sort > "$BASE" || true

# AST scan: every tracked .py file, aliases resolved, one line per site,
# sorted and NOT uniqued.
git -C "$REPO_ROOT" ls-files '*.py' > "$FILES"
python3 - "$REPO_ROOT" "$FILES" <<'PY' | sort > "$FOUND"
import ast
import sys
from pathlib import Path

root = Path(sys.argv[1])
files = [p for p in Path(sys.argv[2]).read_text().split("\n") if p]
out = []

# <pytest>.mark.<skip|skipif|xfail>, including `from pytest import mark`.
MARK_ATTRS = {"skip", "skipif", "xfail"}
# <pytest>.skip|xfail|importorskip(...), including `from pytest import skip`.
CALL_KINDS = {"skip": "pytest.skip", "xfail": "pytest.xfail", "importorskip": "pytest.importorskip"}
# The unittest silencing moves, wherever they appear (self.skipTest(...)).
ALWAYS_ATTRS = {"skipTest", "skipIf", "skipUnless", "expectedFailure"}

for rel in files:
    try:
        tree = ast.parse((root / rel).read_text(encoding="utf-8"))
    except (SyntaxError, ValueError, UnicodeDecodeError, OSError) as exc:
        # OSError: a tracked file missing from a dirty working tree (mid-rebase)
        # is named and skipped, never a traceback; its baseline lines then read
        # as stale and fail the run, so the skip direction stays safe.
        print(f"check-pytest-skips: skipping unparseable file {rel}: {exc}", file=sys.stderr)
        continue

    pytest_mods, mark_names, direct, unittest_mods = set(), set(), {}, set()
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            for a in node.names:
                if a.name == "pytest":
                    pytest_mods.add(a.asname or "pytest")
                elif a.name == "unittest":
                    unittest_mods.add(a.asname or "unittest")
        elif isinstance(node, ast.ImportFrom) and node.module == "pytest" and node.level == 0:
            for a in node.names:
                if a.name == "mark":
                    mark_names.add(a.asname or "mark")
                elif a.name in CALL_KINDS:
                    direct[a.asname or a.name] = CALL_KINDS[a.name]

    def is_pytest_mark(value):
        if isinstance(value, ast.Attribute) and value.attr == "mark":
            return isinstance(value.value, ast.Name) and value.value.id in pytest_mods
        return isinstance(value, ast.Name) and value.id in mark_names

    def site_kind(node):
        if isinstance(node, ast.Attribute):
            if node.attr in MARK_ATTRS and is_pytest_mark(node.value):
                return f"mark.{node.attr}"
            if node.attr == "skip" and isinstance(node.value, ast.Name) and node.value.id in unittest_mods:
                return "unittest.skip"
            if node.attr in ALWAYS_ATTRS:
                return node.attr
        elif isinstance(node, ast.Call):
            f = node.func
            if isinstance(f, ast.Name) and f.id in direct:
                return direct[f.id]
            if (isinstance(f, ast.Attribute) and f.attr in CALL_KINDS
                    and isinstance(f.value, ast.Name) and f.value.id in pytest_mods):
                return CALL_KINDS[f.attr]
        return None

    def check(node, scope):
        kind = site_kind(node)
        if kind:
            out.append(f"{rel}::{scope}::{kind}")

    def scan(node, stack):
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
            # A decorator names the def it decorates, so it takes the def's own
            # full scope (Class.test_y), and it is scanned fully, not just
            # checked: @pytest.mark.skip(...) is a Call whose mark Attribute is
            # the site, so its children must be walked.
            inner = stack + [node.name]
            for dec in node.decorator_list:
                scan(dec, inner)
            decorated = {id(d) for d in node.decorator_list}
            for child in ast.iter_child_nodes(node):
                if id(child) not in decorated:
                    scan(child, inner)
        else:
            check(node, ".".join(stack) if stack else "<module>")
            for child in ast.iter_child_nodes(node):
                scan(child, stack)

    try:
        scan(tree, [])
    except RecursionError:
        print(f"check-pytest-skips: skipping file too deep to walk {rel}", file=sys.stderr)

if out:
    print("\n".join(out))
PY

ADDED="$(comm -23 "$FOUND" "$BASE" || true)"
REMOVED="$(comm -13 "$FOUND" "$BASE" || true)"

if [[ -z "$ADDED" && -z "$REMOVED" ]]; then
  echo "check-pytest-skips: ok ($(wc -l < "$FOUND" | tr -d ' ') site(s) match the baseline)"
  exit 0
fi

if [[ -n "$ADDED" ]]; then
  echo "check-pytest-skips: NEW pytest skip site(s) not in the baseline:" >&2
  while IFS= read -r key; do
    [[ -z "$key" ]] && continue
    printf '  %s  # <why this skip is not a silenced failure>\n' "$key" >&2
  done <<< "$ADDED"
  echo "  Add each line to scripts/ci/pytest-skips-baseline.txt with a real reason," >&2
  echo "  or make the test pass instead of silencing it." >&2
fi
if [[ -n "$REMOVED" ]]; then
  echo "check-pytest-skips: baseline lists skip site(s) no longer in the tree:" >&2
  while IFS= read -r key; do
    [[ -z "$key" ]] && continue
    printf '  %s\n' "$key" >&2
  done <<< "$REMOVED"
  echo "  Delete the stale line(s) from the baseline." >&2
fi
exit 1
