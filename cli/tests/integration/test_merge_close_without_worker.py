"""A merged PR's node closes with no worker alive (x-18c5).

Specimen: PR 1797 merged, the worker was stopped four minutes later, and its
node read in_review until a peer ran reconcile by hand. The merge verb closes
the node in a child bound to the worker's process group
(``FNO_DIE_WITH_PARENT``), so a killed worker takes the closer with it. This
test reproduces that kill window and proves the daemon arm's command - the
bare ``fno backlog reconcile --json`` - is what closes the node afterwards.

The merge child runs REAL code in a fresh interpreter (monkeypatches do not
cross a process boundary): the graph pins through ``$FNO_CONFIG``, and PATH
stubs stand in for ``gh`` (the PR url query) and ``fno-py`` (the bounded
reconcile child, here hung). The sweep step runs in-process with the shared
reconcile test stubs (from test_backlog_reconcile, imported not copied).
"""
from __future__ import annotations

import json
import os
import signal
import subprocess
import sys
import textwrap
import time
from pathlib import Path

import pytest

from fno.cli import app
from fno.graph import _reconcile as rec

from tests.integration.test_backlog_reconcile import (  # noqa: F401  (cli_env fixture)
    _make_graph,
    _node,
    _read_entries,
    _stub_query,
    cli_env,
    runner,
)

PR_A = 910
PR_B = 911
NODE_A = "ab-killwin"
NODE_B = "ab-ctrl"


@pytest.fixture(autouse=True)
def _hermetic_gh(monkeypatch):
    """The in-process sweep below must never shell gh (the child's gh is a
    PATH stub; this covers the parent). Mirrors the autouse fixtures in
    test_backlog_reconcile, which do not reach this module."""
    monkeypatch.setattr(rec, "fetch_recent_merged_prs", lambda **kw: [])
    monkeypatch.setattr(rec, "list_merged_pr_branches", lambda **kw: [])
    monkeypatch.setattr(rec, "list_open_pr_branches", lambda **kw: [])
    import fno.pr.closure as closure_mod

    def _no_trailer(pr_number, **kw):
        return closure_mod.PrClosureContext(
            number=pr_number, body="", url=None, state="MERGED", merged_at=None,
        )

    monkeypatch.setattr(closure_mod, "fetch_pr_closure_context", _no_trailer)


def _write_stub(bin_dir: Path, name: str, body: str) -> None:
    p = bin_dir / name
    p.write_text("#!/bin/sh\n" + body + "\n")
    p.chmod(0o755)


def test_merge_killed_mid_reconcile_stays_open_until_the_bare_sweep(
    cli_env, monkeypatch, tmp_path
):
    """AC3-HP + AC3-ERR: the kill window, then the arm's command closes."""
    graph_path, _sentinel_dir = cli_env
    _make_graph(graph_path, [
        _node(NODE_A, pr_number=PR_A, status="in_review"),
        _node(NODE_B, pr_number=PR_B, status="in_review"),
    ])

    # --- Step 1: the repro. A merge whose reconcile child never finishes. ---
    bin_dir = tmp_path / "stub-bin"
    bin_dir.mkdir()
    started_marker = tmp_path / "fno-py-started"
    _write_stub(bin_dir, "gh", f'echo "https://github.com/test-owner/test-repo/pull/{PR_A}"')
    _write_stub(bin_dir, "fno-py", f'touch "{started_marker}"\nsleep 60')

    config = tmp_path / "config.toml"
    config.write_text(textwrap.dedent(f"""
        [paths]
        graph_json = "{graph_path}"
    """).lstrip())
    tmp_repo = tmp_path / "repo"
    tmp_repo.mkdir()

    child_code = textwrap.dedent(f"""
        from fno.pr._merge import _on_confirmed_merge
        _on_confirmed_merge({PR_A}, cwd=r"{tmp_repo}")
    """).lstrip()
    child_env = dict(os.environ, FNO_CONFIG=str(config), PATH=f"{bin_dir}{os.pathsep}{os.environ['PATH']}")
    out_file = tmp_path / "child.out"
    with open(out_file, "wb") as out:
        child = subprocess.Popen(
            [sys.executable, "-c", child_code],
            env=child_env,
            stdout=out,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        try:
            # The repro is honest only if the merge reached its reconcile leg:
            # wait for the hung child, then SIGKILL the whole group - the
            # worker-stopped-mid-merge shape, closer than "one second" and
            # immune to import-time jitter.
            deadline = time.monotonic() + 30
            while not started_marker.exists():
                assert child.poll() is None, f"merge child exited early: see {out_file}"
                assert time.monotonic() < deadline, "fno-py stub never started"
                time.sleep(0.05)
            os.killpg(child.pid, signal.SIGKILL)
        finally:
            child.wait(timeout=30)

    entries = {e["id"]: e for e in _read_entries(graph_path)}
    assert entries[NODE_A]["completed_at"] is None, (
        "the kill took the closer with it: the node must still be open"
    )
    assert entries[NODE_A]["status"] == "in_review"

    # --- Step 2: the arm's command. The exact bare sweep, in-process. ---
    monkeypatch.setattr(rec, "query_pr_merge_state", _stub_query({PR_A: "MERGED", PR_B: "CLOSED"}))
    result = runner.invoke(app, ["backlog", "reconcile", "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    closed_ids = {c["node_id"] for c in payload["closed"]}
    assert closed_ids == {NODE_A}, f"payload closed: {closed_ids}"

    entries = {e["id"]: e for e in _read_entries(graph_path)}
    assert entries[NODE_A]["completed_at"] is not None
    assert entries[NODE_A]["status"] == "done"
    # Control: a CLOSED (not merged) PR closes nothing.
    assert entries[NODE_B]["completed_at"] is None
    assert entries[NODE_B]["status"] == "in_review"
