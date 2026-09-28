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
            body = json.dumps({"state": s, "html_url": f"https://github.com/o/r/pull/{n}"})
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
