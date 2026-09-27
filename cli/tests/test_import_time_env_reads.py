"""No test module reads an ambient env var at import time.

The shared conftest swaps ``os.environ`` for ``neutralise()``'s output at its
OWN import, before any test module loads, so a module-level read of an ambient
name always sees it unset. Three tests shipped exactly that bug: two opt-in
live journeys skipped for everyone who set their flag, and a front-door
liveness branch never ran. This file is the guard for the class.

Import-time scope means the module body, class bodies, decorators and default
arguments - the code that runs while the module loads. Reads inside a function
or lambda body run later, per test, and are fine.

conftest.py files are exempt: they run before (and own) the swap, so a
pre-sweep capture there is the point, not a bug.
"""
from __future__ import annotations

import ast
import tempfile
from collections.abc import Sequence
from pathlib import Path

import pytest

from fno.hermetic import classify, neutralise

TESTS_ROOT = Path(__file__).resolve().parent


def _is_environ(node: ast.AST) -> bool:
    if isinstance(node, ast.Name):
        return node.id == "environ"
    return isinstance(node, ast.Attribute) and node.attr == "environ"


def _literal_key(args: Sequence[ast.expr]) -> str | None:
    if args and isinstance(args[0], ast.Constant) and isinstance(args[0].value, str):
        return args[0].value
    return None


def _env_read_name(node: ast.AST) -> str | None:
    """The env name a read shape carries, or None. Non-literal keys are skipped:
    the guard only fails on names it can classify, never on guesses."""
    if isinstance(node, ast.Subscript):
        if _is_environ(node.value):
            return _literal_key([node.slice])
        return None
    if not isinstance(node, ast.Call):
        return None
    func = node.func
    if isinstance(func, ast.Name) and func.id == "getenv":
        return _literal_key(node.args)
    if isinstance(func, ast.Attribute):
        if func.attr == "getenv" and isinstance(func.value, ast.Name) and func.value.id == "os":
            return _literal_key(node.args)
        if func.attr == "get" and _is_environ(func.value):
            return _literal_key(node.args)
    return None


def _import_time_reads(tree: ast.Module) -> list[tuple[int, str]]:
    """(lineno, env name) for every literal env read in an import-time scope."""
    reads: list[tuple[int, str]] = []

    def visit(node: ast.AST, at_import: bool) -> None:
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            for dec in node.decorator_list:  # decorators run while the module loads
                visit(dec, at_import)
            args = node.args
            for default in (*args.defaults, *(d for d in args.kw_defaults if d)):
                visit(default, at_import)
            for stmt in node.body:
                visit(stmt, False)
            return
        if isinstance(node, ast.Lambda):
            args = node.args
            for default in (*args.defaults, *(d for d in args.kw_defaults if d)):
                visit(default, at_import)
            visit(node.body, False)
            return
        if isinstance(node, (ast.Assign, ast.Delete)):
            # store targets (os.environ["X"] = ... / del os.environ["X"]) are
            # writes, not reads; an Assign's value still runs at import
            if isinstance(node, ast.Assign):
                visit(node.value, at_import)
            return
        if at_import and isinstance(node, (ast.Call, ast.Subscript)):
            name = _env_read_name(node)
            if name is not None:
                reads.append((node.lineno, name))
        for child in ast.iter_child_nodes(node):
            visit(child, at_import)

    visit(tree, True)
    return reads


def _all_import_time_reads() -> dict[str, list[str]]:
    """{env name -> [file:line, ...]} across the pytest tree."""
    found: dict[str, list[str]] = {}
    for path in sorted(TESTS_ROOT.rglob("*.py")):
        if path.name == "conftest.py" or "__pycache__" in path.parts:
            continue
        tree = ast.parse(path.read_text(errors="replace"), filename=str(path))
        for lineno, name in _import_time_reads(tree):
            found.setdefault(name, []).append(f"{path.relative_to(TESTS_ROOT)}:{lineno}")
    return found


def test_the_probe_sees_the_opt_in_seams():
    """Positive control: the walker must find the two documented opt-in reads,
    or the guard below is green while measuring nothing."""
    found = _all_import_time_reads()
    missing = {"FNO_CODEX_LIVE", "FNO_LIVE_RELAY"} - set(found)
    assert not missing, f"env-read probe found nothing for {sorted(missing)}; the walker is stale"


def test_no_test_module_reads_an_ambient_env_at_import():
    """The guard itself."""
    ambient = {
        name: sites
        for name, sites in _all_import_time_reads().items()
        if classify(name) == "ambient"
    }
    if ambient:
        lines = [
            f"  {name}  (read at {', '.join(sites)})"
            for name, sites in sorted(ambient.items())
        ]
        pytest.fail(
            "These test modules read an ambient env var at import time. The "
            "shared conftest swaps os.environ before any test module loads, so "
            "each read always sees the name unset:\n"
            + "\n".join(lines)
            + "\n\nFix each one: either name the var in hermetic._RUNNER_PASSTHROUGH "
            "(an operator opt-in that must survive the sandbox) or read it at "
            "test time through the conftest seam that caught it before the swap.",
            pytrace=False,
        )


def test_opt_in_live_flags_survive_the_sandbox():
    """The two live journeys stay runnable for whoever sets their flag, while
    the session-marker strip stands."""
    out = neutralise(
        {"FNO_CODEX_LIVE": "1", "FNO_LIVE_RELAY": "1", "CLAUDE_CODE_SESSION_ID": "sess-1"},
        Path(tempfile.mkdtemp()),
    )
    assert out.get("FNO_CODEX_LIVE") == "1"
    assert out.get("FNO_LIVE_RELAY") == "1"
    assert "CLAUDE_CODE_SESSION_ID" not in out
