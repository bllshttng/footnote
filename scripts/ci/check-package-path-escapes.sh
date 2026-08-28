#!/usr/bin/env bash
# scripts/ci/check-package-path-escapes.sh
#
# Condition: an expression in the installed package that roots a repo-tree
# path at parents[N] of __file__ escapes the package root. The wheel ships
# cli/src/fno only - no scripts/, no hooks/ - so such an expression either
# crashes (module scope, the x-3b05 import-time FileNotFoundError) or
# silently disables the leg it feeds (an is_file() guard that quietly
# reads "unchecked"/skip/UNKNOWN on every installed copy). CI never saw it
# because a source checkout always has the tree the expression escapes to.
#
# Rule: inside cli/src/fno (except paths.py, the sanctioned resolver), a
# `/`-division chain rooted at __file__.parents[N] joined with a literal
# naming a repo-only top-level directory is a finding. Those trees are the
# resolver's job: paths.resolve_plugin_script validates the offset, honors
# the env hint and the persisted pointer, and degrades honestly.
#
# Not caught (known limit, same honesty as check-reachable-paths.sh): a
# repo path built without a literal escape segment (variables, joinpath on
# an already-escaped root) is invisible to this rule. The rule pays for the
# shape that shipped broken four times, not every conceivable cousin.
#
# Run:  bash scripts/ci/check-package-path-escapes.sh [--self-test] [src-root]
# Exit: 0 clean, 1 findings or a control that did not fire, 2 misuse.
#
# Proof obligation on any fix PR that touches this file: run the check
# against the PRE-fix tree first and record what it found. A check that
# reports zero on an already-fixed tree proves nothing about the check.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

MODE="check"
if [[ "${1:-}" == "--self-test" ]]; then
  MODE="self-test"; shift
elif [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  awk '/^set -uo pipefail/ {exit} {print}' "$0" | sed -e '$d'; exit 0
elif [[ "${1:-}" == -* ]]; then
  echo "check-package-path-escapes: unknown argument: $1 (use --self-test)" >&2; exit 2
fi
SRC_ROOT="${1:-${REPO_ROOT}/cli/src/fno}"

# Repo-only top-level trees: none of these ship in the wheel.
read -r -d '' ESCAPE_DIRS <<'EOF' || true
scripts
hooks
docs
skills
commands
agents
EOF
export ESCAPE_DIRS

run_scan() {
  # $1 = package root to walk, $2 = label for output
  SCAN_ROOT="$1" SCAN_LABEL="$2" python3 - <<'PYSCAN'
import ast
import os
import sys
from pathlib import Path

ESCAPE_DIRS = frozenset(os.environ["ESCAPE_DIRS"].split())
EXEMPT = {"paths.py"}  # the sanctioned resolver; its offset is validated there

def unwraps_to_parents_index(node):
    """The int index if node is <expr>.parents[N] rooted at __file__, else None."""
    node = _strip_path_calls(node)
    if not isinstance(node, ast.Subscript) or not isinstance(node.value, ast.Attribute):
        return None
    if node.value.attr != "parents":
        return None
    if not _touches_dunder_file(node.value.value):
        return None
    sl = node.slice if sys.version_info >= (3, 9) else node.slice.value
    return sl.value if isinstance(sl, ast.Constant) and isinstance(sl.value, int) else None

def _strip_path_calls(node):
    while isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute) \
            and node.func.attr in {"resolve", "absolute", "expanduser"}:
        node = node.func.value
    return node

def _touches_dunder_file(node):
    for sub in ast.walk(node):
        if isinstance(sub, ast.Name) and sub.id == "__file__":
            return True
    return False

def division_chain(node):
    """Flatten a `/` division tree into its operand leaves."""
    node = _strip_path_calls(node)
    if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Div):
        return division_chain(node.left) + division_chain(node.right)
    return [node]

def escape_literal(node):
    """The literal's first path segment if node is a Constant naming an escape dir."""
    if not isinstance(node, ast.Constant) or not isinstance(node.value, str):
        return None
    first = node.value.lstrip("./").split("/")[0]
    return first if first in ESCAPE_DIRS else None

def scan_file(path):
    tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
    seen = set()
    findings = []
    for node in ast.walk(tree):
        for operand in division_chain(node) if isinstance(node, ast.BinOp) else []:
            if unwraps_to_parents_index(operand) is None:
                continue
            key = (path, operand.lineno)
            if key in seen:
                break
            for other in division_chain(node):
                lit = escape_literal(other)
                if lit and unwraps_to_parents_index(other) is None:
                    seen.add(key)
                    findings.append((path, operand.lineno, lit))
                    break
            break
    return findings

root = Path(os.environ["SCAN_ROOT"])
py_files = sorted(p for p in root.rglob("*.py") if p.name not in EXEMPT)
findings = []
for pf in py_files:
    findings.extend(scan_file(pf))

label = os.environ["SCAN_LABEL"]
for path, lineno, lit in findings:
    print(f"check-package-path-escapes: {label}{path.relative_to(root)}:{lineno} "
          f"joins parents[N] with repo-only '{lit}/' - use paths.resolve_plugin_script")
if findings:
    print(f"check-package-path-escapes: {len(findings)} package-root escape(s) in {label}",
          file=sys.stderr)
    sys.exit(1)
print(f"check-package-path-escapes: clean ({len(py_files)} files in {label})")
PYSCAN
}

if [[ "$MODE" == "self-test" ]]; then
  TMP="$(mktemp -d)"
  trap 'rm -rf "$TMP"' EXIT
  mkdir -p "$TMP/pkg"
  # Positive control: the exact shipped shape. A check that cannot fire is
  # decorative, and a zero on the fixed tree then proves nothing.
  cat > "$TMP/pkg/bad.py" <<'EOF'
from pathlib import Path
script = Path(__file__).resolve().parents[3] / "scripts" / "lib" / "thing.sh"
EOF
  # Negative controls: package-internal parents use, literal not rooted at
  # __file__, and an escape-free variable join must all stay silent.
  cat > "$TMP/pkg/good.py" <<'EOF'
import os
from pathlib import Path
sibling = Path(__file__).resolve().parents[1] / "sibling.py"
data = Path("scripts") / "lib" / "thing.sh"
root = os.environ.get("REPO_ROOT") or "/elsewhere"
p = Path(root) / "scripts" / "lib" / "thing.sh"
EOF
  if run_scan "$TMP/pkg" "self-test:"; then
    echo "check-package-path-escapes: self-test FAILED - control escaped detection" >&2
    exit 1
  fi
  rm "$TMP/pkg/bad.py"
  run_scan "$TMP/pkg" "self-test:" || { echo "check-package-path-escapes: self-test FAILED - false positive" >&2; exit 1; }
  echo "check-package-path-escapes: self-test ok (control fires, negatives silent)"
  exit 0
fi

[[ -d "$SRC_ROOT" ]] || { echo "check-package-path-escapes: no package at $SRC_ROOT" >&2; exit 1; }
run_scan "$SRC_ROOT" ""
