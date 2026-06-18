#!/usr/bin/env bash
# lint-flock-pattern.sh — forbid the pre-#316 dual-read flock pattern.
#
# US4-gemini Wave 1.2 (AC2-UI). Forbids any function in
# cli/src/fno/agents/dispatch.py that directly co-occurs both
# `hold_agent_lock(` and `_resolve_registry_entry(` in its body, EXCEPT
# the helper `with_agent_lock_and_entry` itself which is the sanctioned
# place to call both.
#
# Why this exists: the original stop_agent / rm_agent implementations
# pre-loaded the registry entry, acquired the per-agent flock, then
# re-loaded the entry post-lock. The pre-flock snapshot was a TOCTOU
# race seed — a future contributor that forgot the post-lock re-read
# would silently operate on stale provider/short_id. The
# `with_agent_lock_and_entry` context manager moves the pre+post pair
# into one place so the dual-read can never drift apart. This lint
# script makes that invariant enforceable in CI.
#
# Allowed call sites for `hold_agent_lock`:
#   - `with_agent_lock_and_entry` (the helper itself).
#
# Ask paths (dispatch_ask / _codex_create_path / _codex_followup_path)
# intentionally do NOT use the new helper — they take the registry
# optimistic-read pattern that runs OUTSIDE the per-agent flock. The
# lint script allows ask paths because they do not call
# `_resolve_registry_entry` directly either.
#
# Exit code: 0 on clean dispatch.py, 1 on any violation.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DISPATCH_PY="${1:-${REPO_ROOT}/cli/src/fno/agents/dispatch.py}"

if [[ ! -f "$DISPATCH_PY" ]]; then
    echo "lint-flock-pattern: dispatch.py not found at $DISPATCH_PY" >&2
    exit 1
fi

# Walk dispatch.py and inspect each top-level def/contextmanager body
# for co-occurrence. The implementation is grep-based; ast-grep was an
# alternative but pure-python via the stdlib `ast` module avoids the
# extra dependency and produces deterministic output.

python3 - "$DISPATCH_PY" <<'PYEOF'
import ast
import sys
from pathlib import Path

ALLOWED_HOLD_AGENT_LOCK_CALLERS = {"with_agent_lock_and_entry"}

target_path = Path(sys.argv[1])
source = target_path.read_text(encoding="utf-8")
tree = ast.parse(source, filename=str(target_path))


def _function_calls(node: ast.AST) -> set[str]:
    """Return the set of called callable names inside ``node``'s body.

    Reads Name(func=Name(...)) and Name(func=Attribute(...)) call shapes;
    treats both `hold_agent_lock(...)` and `foo.hold_agent_lock(...)`
    as a match so a future refactor that moves the helper to a sub-module
    still triggers the lint.
    """
    names: set[str] = set()
    for child in ast.walk(node):
        if not isinstance(child, ast.Call):
            continue
        func = child.func
        if isinstance(func, ast.Name):
            names.add(func.id)
        elif isinstance(func, ast.Attribute):
            names.add(func.attr)
    return names


violations: list[str] = []
for node in ast.walk(tree):
    if not isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
        continue
    fn_name = node.name
    if fn_name in ALLOWED_HOLD_AGENT_LOCK_CALLERS:
        continue
    calls = _function_calls(node)
    if "hold_agent_lock" in calls and "_resolve_registry_entry" in calls:
        violations.append(
            f"{target_path.name}:{node.lineno}: function {fn_name!r} calls "
            "both hold_agent_lock and _resolve_registry_entry directly — "
            "use with_agent_lock_and_entry(name) to encapsulate the pair."
        )

if violations:
    print("lint-flock-pattern: violations:", file=sys.stderr)
    for v in violations:
        print(f"  {v}", file=sys.stderr)
    print(
        "\nFix: replace the open-coded pre+post _resolve_registry_entry + "
        "hold_agent_lock block with `with with_agent_lock_and_entry(name) "
        "as (_lock, existing):`.",
        file=sys.stderr,
    )
    sys.exit(1)

print("lint-flock-pattern: ok (no co-occurrence outside helper)")
PYEOF
