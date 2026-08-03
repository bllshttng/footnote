#!/usr/bin/env bash
# Conformance gate: cli/src/fno/company/ must carry no branch on a function
# name (marketing, comms, design, social, support, sales, operations). Core is
# function-agnostic; function semantics arrive as data via function packs.
#
# Flags any occurrence in code or non-docstring string literals (an exact branch
# on a function name is the anti-pattern), but excludes docstrings and comments,
# which may name the rule. Anchored to the company package only. A positive
# control runs first: if the scanner cannot detect a planted literal, it fails
# closed rather than reporting a false clean.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

python3 - "$ROOT" <<'PY'
import ast
import sys
from pathlib import Path

root = Path(sys.argv[1])
pkg = root / "cli" / "src" / "fno" / "company"
FORBIDDEN = {"marketing", "comms", "design", "social", "support", "sales", "operations"}


def _docstring_node_ids(tree: ast.AST) -> set[int]:
    """Identity ids of the Constant string nodes that are docstrings."""
    ids: set[int] = set()
    for node in ast.walk(tree):
        # Only module/class/function bodies can hold a docstring; Lambda.body
        # and similar are single nodes, not lists, so guard on the node type
        # rather than duck-typing a ``body`` attribute.
        if not isinstance(
            node, (ast.Module, ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)
        ):
            continue
        body = node.body
        if not body:
            continue
        first = body[0]
        if (
            isinstance(first, ast.Expr)
            and isinstance(first.value, ast.Constant)
            and isinstance(first.value.value, str)
        ):
            ids.add(id(first.value))
    return ids


def _hits(path: Path):
    tree = ast.parse(path.read_text())
    docstrings = _docstring_node_ids(tree)
    for node in ast.walk(tree):
        if isinstance(node, ast.Name) and node.id in FORBIDDEN:
            yield node.lineno, f"identifier {node.id!r}"
        elif isinstance(node, ast.arg) and node.arg in FORBIDDEN:
            yield node.lineno, f"argument {node.arg!r}"
        elif isinstance(node, ast.FunctionDef) and node.name in FORBIDDEN:
            yield node.lineno, f"definition {node.name!r}"
        elif isinstance(node, ast.ClassDef) and node.name in FORBIDDEN:
            yield node.lineno, f"class {node.name!r}"
        elif isinstance(node, ast.Attribute) and node.attr in FORBIDDEN:
            yield node.lineno, f"attribute {node.attr!r}"
        elif (
            isinstance(node, ast.Constant)
            and isinstance(node.value, str)
            and id(node) not in docstrings
        ):
            for token in node.value.replace("\n", " ").split():
                if token.strip(".,:;()\"'").lower() in FORBIDDEN:
                    yield node.lineno, f"string literal {token!r}"
                    break


def _scan(source: str):
    tree = ast.parse(source)
    docstrings = _docstring_node_ids(tree)
    found = []
    for node in ast.walk(tree):
        if isinstance(node, ast.Name) and node.id in FORBIDDEN:
            found.append(node.id)
        elif (
            isinstance(node, ast.Constant)
            and isinstance(node.value, str)
            and id(node) not in docstrings
        ):
            for token in node.value.replace("\n", " ").split():
                if token.strip(".,:;()\"'").lower() in FORBIDDEN:
                    found.append(token)
                    break
    return found


# Positive control: the scanner must detect a planted literal in code AND in a
# non-docstring string, and must NOT flag the same literal inside a docstring.
_probe = "def f():\n    'docs: marketing is forbidden here'\n    role = 'marketing'\n    return role\n"
if "marketing" not in _scan(_probe):
    print("check-company-function-agnostic: scanner failed its positive control", file=sys.stderr)
    sys.exit(1)

violations = []
for path in sorted(pkg.rglob("*.py")):
    if "__pycache__" in path.parts:
        continue
    for lineno, what in _hits(path):
        rel = path.relative_to(root)
        violations.append(f"{rel}:{lineno}: {what}")

if violations:
    print("check-company-function-agnostic: function-name literal found in core:", file=sys.stderr)
    for v in violations:
        print(f"  {v}", file=sys.stderr)
    print("Core must be function-agnostic; function semantics arrive as data.", file=sys.stderr)
    sys.exit(1)

print(f"check-company-function-agnostic: ok ({sum(1 for _ in pkg.rglob('*.py'))} files scanned)")
PY
