#!/usr/bin/env python3
"""Integration smoke test for the scripts/roadmap-tasks.py compatibility shim.

Verifies the shim correctly routes to fno.graph. Unit tests for the
underlying graph logic have been ported to cli/tests/unit/test_graph_*.py.

Run: python3 tests/test_graph.py   OR   pytest tests/test_graph.py
"""
import atexit
import json
import re
import subprocess
import sys
from pathlib import Path

SCRIPT = Path(__file__).parent.parent / "scripts" / "roadmap-tasks.py"
CLI_DIR = Path(__file__).parent.parent / "cli"

# The shim is invoked via sys.executable, so a bare `python3 tests/test_graph.py`
# runs it under an interpreter with no typer/pydantic and every case dies in an
# import error. Re-exec under the project interpreter so the documented
# invocation works instead of silently testing nothing.
try:
    import typer  # noqa: F401
except ModuleNotFoundError:
    import os
    os.execvp("uv", ["uv", "run", "--project", str(CLI_DIR), "python", __file__])
GRAPH_PATH = Path.home() / ".fno" / "graph.json"
BACKUP_PATH = Path.home() / ".fno" / "graph.json.test-backup"

passed = 0
failed = 0


def _restore_graph():
    if GRAPH_PATH.exists():
        GRAPH_PATH.unlink()
    if BACKUP_PATH.exists():
        BACKUP_PATH.rename(GRAPH_PATH)


atexit.register(_restore_graph)

# Crash recovery for prior hard-killed runs
if BACKUP_PATH.exists() and GRAPH_PATH.exists():
    print(
        f"Warning: both {GRAPH_PATH.name} and {BACKUP_PATH.name} present at "
        "test load - prior run was killed before restore. Recovering.",
        file=sys.stderr,
    )
    GRAPH_PATH.unlink()
    BACKUP_PATH.rename(GRAPH_PATH)
elif BACKUP_PATH.exists() and not GRAPH_PATH.exists():
    BACKUP_PATH.rename(GRAPH_PATH)

if GRAPH_PATH.exists():
    GRAPH_PATH.rename(BACKUP_PATH)


def run(args: list[str]) -> tuple[str, int]:
    result = subprocess.run(
        [sys.executable, str(SCRIPT)] + args,
        capture_output=True, text=True,
    )
    return result.stdout.strip(), result.returncode


def cleanup():
    if GRAPH_PATH.exists():
        GRAPH_PATH.unlink()


def test(name):
    def decorator(fn):
        global passed, failed
        cleanup()
        try:
            fn()
            print(f"  PASS: {name}")
            passed += 1
        except AssertionError as e:
            print(f"  FAIL: {name} - {e}")
            failed += 1
        except Exception as e:
            print(f"  ERROR: {name} - {type(e).__name__}: {e}")
            failed += 1
        finally:
            cleanup()
        return fn
    return decorator


# --- Smoke tests: shim routes correctly to fno.graph ---

@test("shim: ready returns valid JSON array")
def _():
    out, rc = run(["ready", "--all"])
    assert rc == 0, f"exit {rc}: {out}"
    data = json.loads(out)
    assert isinstance(data, list), f"expected list, got {type(data)}"


@test("shim: add creates a node with a <prefix>-<hex> ID")
def _():
    out, rc = run(["add", "Smoke Test Feature", "--project", "smoke"])
    assert rc == 0, f"exit {rc}: {out}"
    data = json.loads(out)
    # The prefix and hex width are config-driven (config.backlog.id_prefix /
    # id_hex_width), so assert the shape rather than a hardcoded "ab-".
    assert re.fullmatch(r"[a-z]+-[0-9a-f]{4,8}", data["id"]), (
        f"ID should be <prefix>-<4..8 hex>: {data['id']}"
    )
    assert data["title"] == "Smoke Test Feature"


@test("shim: next returns null on empty graph")
def _():
    out, rc = run(["next", "--all"])
    assert rc == 0, f"exit {rc}: {out}"
    assert out == "null", f"expected null, got {out!r}"


@test("shim: get returns node JSON after add")
def _():
    add_out, _ = run(["add", "GetSmoke", "--project", "smoke"])
    node_id = json.loads(add_out)["id"]
    out, rc = run(["get", node_id])
    assert rc == 0, f"exit {rc}: {out}"
    data = json.loads(out)
    assert data["id"] == node_id
    assert data["title"] == "GetSmoke"


@test("shim: update --completed is rejected, not a silent close")
def _():
    add_out, _ = run(["add", "ToComplete", "--project", "smoke"])
    node_id = json.loads(add_out)["id"]
    _, rc = run(["update", node_id, "--completed"])
    assert rc != 0
    get_out, _ = run(["get", node_id])
    data = json.loads(get_out)
    assert data["status"] != "done"
    assert data["completed_at"] is None


@test("shim: validate passes on clean graph")
def _():
    run(["add", "ValidNode", "--project", "smoke"])
    out, rc = run(["validate"])
    assert rc == 0, f"validate failed: {out}"
    assert "OK" in out or "no issues" in out.lower()


if __name__ == "__main__":
    print(f"\nResults: {passed} passed, {failed} failed")
    sys.exit(0 if failed == 0 else 1)
