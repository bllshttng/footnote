"""Shared sandbox + door helpers for the workflow golden receipts."""
from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path

import pytest

from tests.fixtures.graph_seed import seed_graph

# A fixed session identity so holder-bearing receipts differ only in the
# shape they must keep, never in the holder text between runs.
SESSION_ID = "test-session-golden"


def make_sandbox(tmp_path: Path, entries: list[dict]) -> Path:
    """One isolated state root: config, seeded store, claims root."""
    root = tmp_path / "state"
    root.mkdir()
    (root / "config.toml").write_text(f'state_dir = "{root}"\n')
    (root / "claims").mkdir()
    seed_graph(root / "graph.json", json.dumps(entries))
    return root


def seed_node(id: str, status: str = "in_progress", **kw) -> dict:
    base = {
        "id": id,
        "title": kw.pop("title", f"node {id}"),
        "status": status,
        # Every store mutator parses rows through the typed Node, which
        # requires `type`; a seed without one refuses its own mutation.
        "type": kw.pop("type", "feature"),
        "project": "fno",
        "slug": kw.pop("slug", f"slug-{id.split('-', 1)[1]}"),
        "priority": "p2",
    }
    base.update(kw)
    return base


def door(
    root: Path,
    args: list[str],
    *,
    path_prepend: str | None = None,
) -> tuple[int, str, str]:
    """Run `fno-agents backlog <args>` against the sandbox; skip when this
    checkout has no dev build (the same finder the other door tests use)."""
    from fno.rust_binary import find_dev_binary, resolve_binary

    binary = find_dev_binary() or resolve_binary()
    if binary is None:
        pytest.skip("no fno-agents dev build (cargo build -p fno-agents)")
    env = {
        **os.environ,
        "FNO_CONFIG": str(root / "config.toml"),
        "FNO_GLOBAL_SETTINGS_PATH": "/dev/null",
        "FNO_TRACKER_BACKEND": "graph",
        "FNO_CLAIMS_ROOT": str(root / "claims"),
        "CLAUDECODE_SESSION_ID": SESSION_ID,
    }
    if path_prepend:
        env["PATH"] = f"{path_prepend}:{os.environ['PATH']}"
    proc = subprocess.run(
        [str(binary), "backlog", *args],
        capture_output=True,
        text=True,
        env=env,
        timeout=120,
        cwd=str(root),
    )
    return proc.returncode, proc.stdout, proc.stderr


def warm(root: Path, probe: str) -> None:
    """Absorb the one-time path-migration banner a fresh state root prints,
    so golden assertions see the steady-state streams."""
    door(root, ["get", probe])


def roster_stub(root: Path, rows: list | None = None) -> str:
    """A PATH dir whose `claude` answers one roster listing (empty when no
    rows are given), so the worked fold stays hermetic. Pass e.g. [42] for a
    malformed listing, which the strict fold treats as a refusal."""
    stub = root / "stubbin"
    stub.mkdir(exist_ok=True)
    body = json.dumps({"agents": rows or []})
    script = "#!/bin/sh\nprintf '%s' '" + body.replace("'", "'\\''") + "'\nexit 0\n"
    (stub / "claude").write_text(script)
    (stub / "claude").chmod(0o755)
    return str(stub)


def door_graph(graph: Path, *args: str) -> tuple[int, str, str]:
    """Run a selection verb (next/undispatched) through the binary against a
    fixture-seeded store at graph.parent. Writes config.toml when the fixture
    has none, isolates claims under the state root, and stubs the roster
    listing empty so the worked fold never leaves the sandbox. The door
    splits the streams: selection JSON rides stdout, starvation receipts
    answer on stderr."""
    root = graph.parent
    config = root / "config.toml"
    if not config.exists():
        config.write_text(f'state_dir = "{root}"\n')
    return door(root, list(args), path_prepend=roster_stub(root, []))


def write_gh_stub(tmp_path: Path, body: str = "[]\n") -> str:
    """A PATH dir whose `gh` answers one JSON line to every call, so the
    GitHub-reading verbs stay hermetic."""
    stub_bin = tmp_path / "stubbin"
    stub_bin.mkdir()
    gh = stub_bin / "gh"
    gh.write_text(f"#!/bin/sh\ncat /dev/null\nprintf '{body}'\nexit 0\n")
    gh.chmod(0o755)
    return str(stub_bin)


def write_pr_stub(root: Path, states: dict[int, str] | None = None, *, fail_stderr: str = "") -> Path:
    """A PATH dir whose `gh` answers per-PR JSON (or fails), so the gate reads
    stay hermetic. states: {41: "OPEN"} -> {"state": ..., "html_url": ...}.
    Returns the dir to prepend to PATH (door's path_prepend)."""
    stubbin = root / "stubbin"
    stubbin.mkdir(exist_ok=True)
    if fail_stderr:
        # A failing gh answers nothing: every call fails, which is the shape
        # both the outage and the routing refusal tests need.
        escaped = fail_stderr.replace("'", "'\\''")
        script = "\n".join(
            [
                "#!/bin/sh",
                f"printf '%s' '{escaped}' >&2",
                "exit 1",
            ]
        )
    else:
        arms = []
        for n, s in sorted((states or {}).items()):
            # REST reality: a merged PR is state "closed" with merged true,
            # never a MERGED state. The stub speaks production so the reader's
            # mapping is what the tests prove.
            if str(s).upper() == "MERGED":
                payload = {"state": "closed", "merged": True,
                           "merged_at": "2026-06-01T10:00:00Z",
                           "html_url": f"https://github.com/o/r/pull/{n}"}
            else:
                payload = {"state": str(s).lower(),
                           "html_url": f"https://github.com/o/r/pull/{n}"}
            body = json.dumps(payload)
            arm = '  case "$a" in */pulls/{n}) printf \'{b}\'; exit 0;; esac'
            arms.append(arm.replace("{n}", str(n)).replace("{b}", body))
        script = "\n".join(["#!/bin/sh", 'for a in "$@"; do'] + arms + ["done", "printf '{}'", "exit 0"])
    stub = stubbin / "gh"
    stub.write_text(script)
    stub.chmod(0o755)
    return stubbin


def graph_rows(root: Path) -> list[dict]:
    from fno.graph.api import wire_rows

    return wire_rows(path=root / "graph.json")
