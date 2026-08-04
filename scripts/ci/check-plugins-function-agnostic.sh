#!/usr/bin/env bash
# Conformance gate: cli/src/fno/plugins/ carries no branch on a pack id or a
# function name. A pack is discovered by path and validated by schema; core code
# that keys on "growth-studio" or on marketing/comms/design/social/support/sales/
# operations is the function-coupling defect this gate exists to catch.
#
# It scans Python string LITERALS (not docstrings, not comments) via AST, so an
# informative module docstring that names the first pack is fine while a code
# path that compares against the same literal fails loudly with file:line.
#
# The scan is scoped to cli/src/fno/plugins/ only: pack content under plugins/
# and tests under cli/tests/ legitimately name functions and are out of scope.
#
# Positive control: an empty result is a claim, so before trusting "no matches"
# the script confirms the AST walk reached real content by checking a token known
# to appear in the package (PackManifest). A broken traversal fails closed.
set -euo pipefail

root="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
target="$root/cli/src/fno/plugins"

if [[ ! -d "$target" ]]; then
  echo "check-plugins-function-agnostic: $target not found; the package must exist to be scanned (fail closed)" >&2
  exit 1
fi

python3 - "$target" <<'PY'
import ast
import sys
from pathlib import Path

target = Path(sys.argv[1])
forbidden = [
    "growth-studio",
    "marketing",
    "communications",
    "comms",
    "design",
    "social",
    "support",
    "sales",
    "operations",
]


def docstring_node_ids(tree):
    """Return ast.Constant node ids that are docstrings (module/class/func first stmt)."""
    ids = set()

    def visit(node, get_body):
        body = get_body(node)
        if body and isinstance(body[0], ast.Expr) and isinstance(body[0].value, ast.Constant) and isinstance(body[0].value.value, str):
            ids.add(id(body[0].value))
        for child in ast.iter_child_nodes(node):
            child_body = getattr(child, "body", None)
            if isinstance(child_body, list):
                visit(child, lambda n: n.body)

    visit(tree, lambda n: getattr(n, "body", []))
    return ids


violations = []
files = sorted(target.rglob("*.py"))
for path in files:
    source = path.read_text(encoding="utf-8")
    tree = ast.parse(source, filename=str(path))
    docstring_ids = docstring_node_ids(tree)
    for node in ast.walk(tree):
        if isinstance(node, ast.Constant) and isinstance(node.value, str) and id(node) not in docstring_ids:
            value = node.value.lower()
            hit = next((token for token in forbidden if token in value), None)
            if hit is not None:
                violations.append(f"{path}:{node.lineno}: forbidden literal {hit!r} in {node.value!r}")

# Positive control: confirm the walk reached real package content. PackManifest
# is a class defined in the package, so a zero hit here means the scan is broken.
joined = "\n".join(p.read_text(encoding="utf-8") for p in files)
if "PackManifest" not in joined:
    print("check-plugins-function-agnostic: positive control failed (PackManifest not seen); scan did not reach content", file=sys.stderr)
    sys.exit(1)

if violations:
    print("check-plugins-function-agnostic: cli/src/fno/plugins/ is not function-agnostic", file=sys.stderr)
    for line in violations:
        print("  " + line, file=sys.stderr)
    sys.exit(1)

print("check-plugins-function-agnostic: cli/src/fno/plugins/ carries no pack or function literal")
PY
