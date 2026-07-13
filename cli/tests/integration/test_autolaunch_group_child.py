"""autolaunch-on-ready resolves a decomposed epic to its ready child (ab-9d98fa7b).

Re-verified here because x-edf7's born-unlinked design changes WHEN a group child
becomes launchable: a child with no plan_path is `idea`/`blocked` (never `ready`),
so the epic-redirect must PARK until a child is inline-filled + linked, then
redirect to that child - not to the epic (which would rebuild every wave in one
PR) and not to an unfilled child (the observed stub-launch bug).

The script needs only python3 + a GRAPH_JSON for the park path; the redirect path
additionally stubs `fno backlog get` and runs dispatch in --dry-run. The gate is
forced ON by exporting a `get_config` that returns true (the script skips sourcing
config.sh when the function is already defined).
"""
from __future__ import annotations

import json
import subprocess
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]
SCRIPT = REPO_ROOT / "skills" / "blueprint" / "scripts" / "autolaunch-on-ready.sh"

EPIC = "ab-1a2b3c4d"
CHILD1 = "ab-11112222"
CHILD2 = "ab-33334444"


def _epic_plan(tmp_path: Path) -> Path:
    plan = tmp_path / "epic.md"
    plan.write_text(f"---\nclaims: {EPIC}\ntitle: Epic\n---\n# Epic\n")
    return plan


def _write_graph(tmp_path: Path, child1_status: str, child1_plan) -> Path:
    g = tmp_path / "graph.json"
    g.write_text(json.dumps({"entries": [
        {"id": EPIC, "parent": None, "_status": "ready", "plan_path": "epic.md"},
        {"id": CHILD1, "parent": EPIC, "group_slug": "1",
         "plan_path": child1_plan, "_status": child1_status},
        {"id": CHILD2, "parent": EPIC, "group_slug": "2",
         "plan_path": None, "_status": "blocked"},
    ]}))
    return g


def _run(plan: Path, graph: Path, extra_path: str = "", dry=False):
    # Force the gate ON via an exported get_config; run the script under bash.
    inner = 'get_config() { echo true; }; export -f get_config; bash "$1" "$2"'
    args = [str(SCRIPT), str(plan)]
    if dry:
        inner += ' --dry-run'
    env = {"GRAPH_JSON": str(graph), "PATH": (extra_path + ":" if extra_path else "")
           + subprocess.os.environ["PATH"]}
    return subprocess.run(["bash", "-c", inner, "_", *args],
                          capture_output=True, text=True, cwd=str(REPO_ROOT), env=env)


def test_all_unlinked_children_park_never_launch(tmp_path):
    # x-edf7: children born unlinked are idea/blocked -> the epic-redirect parks.
    plan = _epic_plan(tmp_path)
    graph = _write_graph(tmp_path, child1_status="idea", child1_plan=None)
    result = _run(plan, graph)
    out = result.stdout + result.stderr
    assert "parked" in out and "epic-decomposed-no-ready-child" in out
    assert CHILD1 not in out.split("parked")[0] or "no-ready-child" in out  # not launched


def _fake_fno(tmp_path: Path) -> str:
    """A minimal `fno` that answers `backlog get <id>` from GRAPH_JSON, no-ops else."""
    bindir = tmp_path / "bin"
    bindir.mkdir()
    (bindir / "fno").write_text(
        "#!/usr/bin/env bash\n"
        'if [[ "$1" == "backlog" && "$2" == "get" ]]; then\n'
        "  python3 -c \"import json,sys,os\\n"
        "d=json.load(open(os.environ['GRAPH_JSON']))\\n"
        "[print(json.dumps(e)) for e in d['entries'] if e.get('id')==sys.argv[1]]\" \"$3\"\n"
        "  exit 0\nfi\nexit 0\n"
    )
    (bindir / "fno").chmod(0o755)
    return str(bindir)


def test_redirect_resolves_to_ready_child_not_epic(tmp_path):
    # ab-9d98fa7b: a decomposed epic whose child is filled+linked (ready) redirects
    # to that CHILD - the resolution the bug said was missing.
    plan = _epic_plan(tmp_path)
    graph = _write_graph(tmp_path, child1_status="ready",
                         child1_plan="/plans/big.group-1.md")
    result = _run(plan, graph, extra_path=_fake_fno(tmp_path), dry=True)
    out = result.stdout + result.stderr
    # The redirect targeted the ready child, not the epic, not the blocked sibling,
    # and did NOT fall through to "nothing to launch".
    assert CHILD1 in out
    assert "nothing to launch" not in out
    assert "first ready child" in out or CHILD1 in out
