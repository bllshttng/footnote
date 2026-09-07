"""A declared dispatch_verb must survive the selection projection (x-0961).

`fno backlog advance --epic` and lane-fill select nodes by shelling
`fno backlog ready`, whose output dict once dropped `dispatch_verb` and
`dispatch_brief`. A node declaring `/fno:blueprint` was silently dispatched
as the builtin `/target`, and its brief never reached `TARGET_BRIEF`.

The positive marker is the argv and env handed to `fno agents spawn`, one
hop before the worker's first transcript turn: the command token IS that
turn and the recorded `TARGET_BRIEF` IS the brief the worker reads. The
tests run the REAL selection subprocess (the real `cmd_ready` against a
temp graph) and never patch `_ready_leaf_children`, `_ready_nodes`, or
`_spawn_worker` - patching those injects a double richer than the real
projection, which is exactly the blind spot that let the bug live in
`test_epic_kickoff.py` for its whole history.

Every spawn case asserts the recorder is non-empty BEFORE asserting on its
contents, so a zero can never read as a pass.
"""
from __future__ import annotations

import json
import sys
from pathlib import Path
from types import SimpleNamespace

import pytest

from fno.backlog import advance as adv

BRIEF_SENTINEL = "brief-sentinel-7f31 blueprint-not-target"


class Iso:
    """Isolated state: temp graph via a pinned FNO_CONFIG (the same file the
    selection SUBPROCESS re-reads), tmp claims, armed auto-continue.

    `_spawn_worker` resolves its dispatch config through
    `load_settings_for_repo(<node cwd>)`, a repo-scoped chain that IGNORES
    FNO_CONFIG and would otherwise read this machine's real global config
    (whose allowlist refuses /blueprint and whose merge grant varies). Each
    test repo therefore carries its own `.fno/config.toml` pinning the two
    keys the spawn reads, exactly as a real repo would."""

    def __init__(self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
        self.root = tmp_path
        self.graph = tmp_path / "graph.json"
        cfg = tmp_path / "config.toml"
        cfg.write_text(
            f'state_dir = "{tmp_path / ".fno"}"\n'
            "\n[dispatch]\n"
            'allowed_verbs = ["/target", "/blueprint"]\n'
            "\n[paths]\n"
            f'graph_json = "{self.graph}"\n'
        )
        monkeypatch.setenv("FNO_CONFIG", str(cfg))
        monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path))
        monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
        monkeypatch.setenv("FNO_AUTO_CONTINUE", "1")
        # The wrapper shells `fno_py_cmd()`, which resolves the INSTALLED
        # fno-py off PATH. A regression test must exercise the code under
        # test, so route every wrapper subprocess through this interpreter
        # and the import root it already carries.
        monkeypatch.setattr(
            "fno._subprocess_util.fno_py_cmd",
            lambda: [sys.executable, "-c", "from fno.cli import app; app()"],
        )
        self.events = tmp_path / ".fno" / "events.jsonl"

    def repo(self, name: str = "web", *, git: bool = False) -> Path:
        repo = self.root / name
        fno_dir = repo / ".fno"
        fno_dir.mkdir(parents=True)
        (fno_dir / "config.toml").write_text(
            "[dispatch]\n"
            'allowed_verbs = ["/target", "/blueprint"]\n'
            "\n[auto_merge]\n"
            "enabled = false\n"
        )
        if git:
            (repo / ".git").mkdir()
        return repo


@pytest.fixture
def iso(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Iso:
    return Iso(tmp_path, monkeypatch)


def _write_graph(graph: Path, *, verb: str | None, brief: str | None, cwd: str) -> None:
    """One epic with one ready leaf child; the child declares what it declares."""
    child: dict = {
        "id": "x-BP01",
        "slug": "bp-declared",
        "title": "declares its verb",
        "parent": "x-EPIC",
        "project": "web",
        "status": "ready",
        "domain": "code",
        "priority": "p1",
        "cwd": cwd,
        "created_at": "2026-09-07T00:00:00+00:00",
        "touched_at": "2026-09-07T00:00:00+00:00",
    }
    if verb is not None:
        child["dispatch_verb"] = verb
    if brief is not None:
        child["dispatch_brief"] = brief
    graph.write_text(
        json.dumps(
            {
                "entries": [
                    {"id": "x-EPIC", "title": "verb mission", "project": "fno"},
                    child,
                ]
            }
        )
        + "\n"
    )


def _events(p: Path) -> list[dict]:
    """Envelope rows only: the journal also carries claim-stamp rows (no
    `type` key) from the graph lock stamp."""
    if not p.exists():
        return []
    rows = [json.loads(line) for line in p.read_text().splitlines() if line.strip()]
    return [r for r in rows if isinstance(r, dict) and "type" in r]


def _record_spawns(monkeypatch: pytest.MonkeyPatch) -> list[dict]:
    """Patch subprocess.run in fno.backlog.advance ONLY.

    `backlog ...` argvs delegate to the real subprocess so the shipped
    selection surface runs for real; `agents spawn` argvs are recorded with
    their env and answered with a fake exit-0 thread receipt; `worktree
    ensure` answers the policy=never shape (the repo root itself) so the
    lane path never touches the real filesystem.
    """
    calls: list[dict] = []
    real_run = adv.subprocess.run

    def fake_run(cmd, *args, **kwargs):
        argv = [str(a) for a in cmd]
        if "backlog" in argv:
            return real_run(cmd, *args, **kwargs)
        if "spawn" in argv:
            calls.append({"argv": argv, "env": dict(kwargs.get("env") or {})})
            receipt = json.dumps(
                {"name": "fake-thread", "short_id": "fakebp19", "substrate": "thread"}
            )
            return SimpleNamespace(returncode=0, stdout=receipt, stderr="")
        if "worktree" in argv and "ensure" in argv:
            repo = argv[argv.index("--repo") + 1]
            return SimpleNamespace(returncode=0, stdout=repo, stderr="")
        return real_run(cmd, *args, **kwargs)

    # A shim object, not a patch of subprocess.run itself: advance's module
    # reference is swapped, so the delegation leg still reaches the real
    # subprocess.run and no other module is touched.
    monkeypatch.setattr(adv, "subprocess", SimpleNamespace(run=fake_run))
    return calls


def test_epic_advance_declared_verb_reaches_spawn_argv(iso, monkeypatch):
    """AC4/AC5: a node declaring /fno:blueprint, dispatched through epic
    advance, hands the spawn the rendered verb and the brief on TARGET_BRIEF."""
    repo = iso.repo()
    _write_graph(iso.graph, verb="/fno:blueprint", brief=BRIEF_SENTINEL, cwd=str(repo))
    monkeypatch.setattr(
        "fno.graph._intake.project_root_from_settings",
        lambda project: str(repo) if project == "web" else None,
    )
    monkeypatch.setattr(adv, "_spawn_headroom", lambda provider=None: 2)
    calls = _record_spawns(monkeypatch)

    res = adv.advance_epic("x-EPIC", events_path=iso.events)

    assert calls, "positive control: the spawn instrument ran and recorded"
    assert res.dispatched == ("x-BP01",)
    # /fno:blueprint canonicalizes to /blueprint for the allowlist; the
    # claude command surface renders it verbatim, {id} substituted.
    assert calls[0]["argv"][-1] == "/blueprint x-BP01", calls[0]["argv"]
    assert calls[0]["env"].get("TARGET_BRIEF") == BRIEF_SENTINEL
    # AC7-HP: the receipt names the resolved verb and its source.
    disp = [e for e in _events(iso.events) if e["type"] == "advance_dispatched"]
    assert disp and disp[0]["data"]["verb"] == "/blueprint"
    assert disp[0]["data"]["verb_source"] == "declared"
    # AC10-HP: the worker-to-node join rides the spawn argv.
    argv = calls[0]["argv"]
    assert argv[argv.index("--node") + 1] == "x-BP01"
    assert argv[argv.index("--slug") + 1] == "bp-declared"


def test_epic_advance_undeclared_node_keeps_the_builtin(iso, monkeypatch):
    """AC6-EDGE: a node declaring nothing gets the builtin and no brief -
    the same code path, distinguished only by the declaration."""
    repo = iso.repo()
    _write_graph(iso.graph, verb=None, brief=None, cwd=str(repo))
    monkeypatch.setattr(
        "fno.graph._intake.project_root_from_settings",
        lambda project: str(repo) if project == "web" else None,
    )
    monkeypatch.setattr(adv, "_spawn_headroom", lambda provider=None: 2)
    calls = _record_spawns(monkeypatch)

    res = adv.advance_epic("x-EPIC", events_path=iso.events)

    assert calls, "positive control: the spawn instrument ran and recorded"
    assert res.dispatched == ("x-BP01",)
    assert calls[0]["argv"][-1] == "/target --no-merge x-BP01", calls[0]["argv"]
    assert "TARGET_BRIEF" not in calls[0]["env"]
    # AC8-HP: a legitimate default is named as one, not left to guesswork.
    disp = [e for e in _events(iso.events) if e["type"] == "advance_dispatched"]
    assert disp and disp[0]["data"]["verb"] == "builtin"
    assert disp[0]["data"]["verb_source"] == "none-declared"


def test_field_absent_node_dict_warns_and_names_the_source(iso, monkeypatch, capsys):
    """AC9-EDGE: a node dict built without a dispatch_verb key at all (the
    pre-fix projection) dispatches with verb_source=field-absent and a
    warning on stderr - the tripwire that makes the next lossy projection
    self-report instead of silently substituting the builtin."""
    repo = iso.repo()
    calls = _record_spawns(monkeypatch)
    # The pre-fix projection's shape, constructed directly: eleven keys, no
    # dispatch_verb. _converge_one is called directly because every shipped
    # selection surface now carries the key.
    node_meta = {
        "id": "x-BP01",
        "slug": "bp-declared",
        "title": "t",
        "project": "web",
        "cwd": str(repo),
    }

    res = adv._converge_one(node_meta, str(repo), iso.events, verbose=False)

    assert calls, "positive control: the spawn instrument ran and recorded"
    assert res.decision == "dispatched"
    disp = [e for e in _events(iso.events) if e["type"] == "advance_dispatched"]
    assert disp and disp[0]["data"]["verb"] == "builtin"
    assert disp[0]["data"]["verb_source"] == "field-absent"
    err = capsys.readouterr().err
    assert "x-BP01" in err and "dispatch_verb" in err, err


def test_lane_fill_declared_verb_reaches_spawn_argv(iso, monkeypatch):
    """The lane-fill door (`_ready_nodes` -> `dispatch_lanes`) shells the same
    `fno backlog ready` surface, so it eats declared verbs identically."""
    repo = iso.repo(git=True)
    _write_graph(iso.graph, verb="/fno:blueprint", brief=BRIEF_SENTINEL, cwd=str(repo))
    calls = _record_spawns(monkeypatch)

    receipts = adv.dispatch_lanes(
        1, project="web", events_path=iso.events, claims_root=iso.root
    )

    assert calls, "positive control: the spawn instrument ran and recorded"
    assert [r["status"] for r in receipts] == ["dispatched"], receipts
    assert calls[0]["argv"][-1] == "/blueprint x-BP01", calls[0]["argv"]
    assert calls[0]["env"].get("TARGET_BRIEF") == BRIEF_SENTINEL
