"""The spawn seam's decision is visible: skips name themselves, one event.

x-f1ab folded into x-90a9 task 0.1. Every assertion is on a POSITIVE marker:
a parsed JSON row whose ``kind`` is ``spawn_defaults_applied``, or a stderr
line naming the dropped axis. An absence (no skip line, no route flag) is
always paired with its positive control so a pipeline loss cannot read as a
verdict.
"""
from __future__ import annotations

import io
import json
from pathlib import Path
from typing import Any

import pytest

from fno.agents import events as agent_events
from fno.agents.spawn_defaults import inject_spawn_defaults

# Declared journal isolation: the conftest pin keys on this module's name
# (see _PLAN_JOURNAL_PINNED_MODULES) and the guard
# scripts/ci/check-tests-hermetic-events.sh proves this marker did not rot.
FNO_EVENTS_PATH = "hermetic: this module's journal is pinned per test"


class _Defaults:
    def __init__(
        self,
        provider="",
        model="",
        effort="",
        substrate="",
        permission_mode="",
        route="",
        account="",
        pane_group="",
        lanes=None,
        on_exhausted="",
        by_difficulty=None,
        on_low="prefer_healthy",
        on_unknown="allow",
    ):
        self.provider = provider
        self.model = model
        self.effort = effort
        self.substrate = substrate
        self.permission_mode = permission_mode
        self.route = route
        self.account = account
        self.pane_group = pane_group
        self.lanes = [
            _Defaults(**lane) if isinstance(lane, dict) else lane
            for lane in (lanes or [])
        ]
        self.on_exhausted = on_exhausted
        self.by_difficulty = by_difficulty or {}
        self.on_low = on_low
        self.on_unknown = on_unknown


class _Settings:
    def __init__(self, profiles=None, model_routing=None, max_lanes=None, **kw):
        prof = {k: _Defaults(**v) for k, v in (profiles or {}).items()}
        self.agents = type(
            "A",
            (),
            {
                "defaults": _Defaults(**kw),
                "profiles": prof,
                "max_lanes": max_lanes or {},
            },
        )()
        self.model_routing = model_routing


@pytest.fixture
def journal(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """This test's own journal: FNO_EVENTS_PATH plus a forced emit path."""
    target = tmp_path / "events.jsonl"
    monkeypatch.setenv("FNO_EVENTS_PATH", str(target))
    real_emit = agent_events.emit
    monkeypatch.setattr(
        agent_events,
        "emit",
        lambda kind, **data: real_emit(kind, path=target, **data),
    )
    return target


def _decision(journal: Path) -> list[dict[str, Any]]:
    if not journal.exists():
        return []
    rows = [
        json.loads(line)
        for line in journal.read_text().splitlines()
        if line.strip()
    ]
    return [r for r in rows if r.get("kind") == "spawn_defaults_applied"]


def _inject(args, err=None, env=None, profiles=None, model_routing=None, **cfg):
    return inject_spawn_defaults(
        args,
        settings=_Settings(profiles=profiles, model_routing=model_routing, **cfg),
        stderr=err,
        env=env or {},
    )


def _grid_candidate(monkeypatch, candidate, chain):
    """Force the no-lanes grid branch to return one candidate.

    resolve_slot is imported inside inject_spawn_defaults, so patching the
    route_resolve module attribute reaches it. _grid_node is patched too: the
    grid only runs when a node entry exists, and a synthetic one keeps this
    fixture off the live graph.
    """
    import fno.route_resolve as rr

    import fno.agents.spawn_defaults as sd

    class _Inv:
        pass

    monkeypatch.setattr(
        sd, "_grid_node", lambda toks, env=None: {"id": "x-test", "difficulty": "high"}
    )
    monkeypatch.setattr(rr, "resolve_inventory", lambda: _Inv())
    monkeypatch.setattr(rr, "runtime_capacity", lambda inventory=None: {})
    monkeypatch.setattr(rr, "resolve_slot", lambda *a, **k: (candidate, chain))


def test_route_skip_names_caller_route_rung_and_reason(journal: Path) -> None:
    """AC17. A profile route dropped for the caller's --route names all three."""
    err = io.StringIO()
    out = _inject(
        ["spawn", "--provider", "zai", "--name", "p", "/target x-90a9"],
        err=err,
        profiles={"target": {"route": "zai,glm-5.3-flash[1m]"}},
    )
    text = err.getvalue()
    assert "--route" not in out
    assert "route skipped" in text
    assert "agents.profiles.target.route" in text
    assert "--provider 'zai'" in text
    rows = _decision(journal)
    assert len(rows) == 1, rows
    suppressed = rows[0]["suppressed"]
    route_rows = [s for s in suppressed if s[0] == "route"]
    assert route_rows, rows
    assert route_rows[0][1] == "zai,glm-5.3-flash[1m]"
    assert route_rows[0][2] == "agents.profiles.target"
    assert "provider" in route_rows[0][3]


def test_route_skip_names_explicit_route_and_model(journal: Path) -> None:
    """AC17 variant: --route on the argv and --model on the argv each named."""
    import io as _io  # noqa: F401 - kept local for symmetry with siblings

    for extra, marker in ((["--route", "other,v"], "--route"), (["--model", "m1"], "--model")):
        err = _io.StringIO()
        out = _inject(
            ["spawn", *extra, "--name", "p", "/target x-90a9"],
            err=err,
            profiles={"target": {"route": "zai,glm-5.3-flash[1m]"}},
        )
        assert "--route zai" not in " ".join(out)
        assert "route skipped" in err.getvalue()
        assert marker in err.getvalue()
        rows = _decision(journal)
        assert rows and any(
            s[0] == "route" and "caller passed" in s[3] for s in rows[-1]["suppressed"]
        )


def test_route_skip_names_grid_reason_on_defaults_rung(
    journal: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC16. Grid suppression is reachable only from the DEFAULTS rung."""
    import io as _io  # noqa: F401 - kept local for symmetry with siblings

    _grid_candidate(
        monkeypatch,
        {"harness": "claude", "model": "claude-opus-5", "effort": "high"},
        ["grid=difficulty claude/claude-opus-5"],
    )
    err = _io.StringIO()
    out = _inject(
        ["spawn", "--name", "p", "/target x-90a9"],
        err=err,
        route="zai,glm-5.3-flash[1m]",
    )
    text = err.getvalue()
    assert "--route" not in out
    assert "route skipped" in text
    assert "agents.defaults.route" in text
    assert "capacity grid" in text
    rows = _decision(journal)
    assert rows and any(
        s[0] == "route" and "capacity grid" in s[3] for s in rows[-1]["suppressed"]
    )


def test_route_resolved_empty_is_recorded_not_warned(journal: Path) -> None:
    """AC18. An empty profile route is a resolved-empty fact, not a skip."""
    import io as _io  # noqa: F401 - kept local for symmetry with siblings

    err = _io.StringIO()
    out = _inject(
        ["spawn", "--name", "p", "/target x-90a9"],
        err=err,
        profiles={"target": {"route": ""}},
    )
    assert "route skipped" not in err.getvalue()
    rows = _decision(journal)
    assert len(rows) == 1, rows
    assert rows[0]["resolved"]["route"] == {"value": "", "rung": None}


def test_route_applied_names_source_and_no_skip(journal: Path) -> None:
    """AC19. No suppression: the route is injected with its source rung."""
    import io as _io  # noqa: F401 - kept local for symmetry with siblings

    err = _io.StringIO()
    out = _inject(
        ["spawn", "--name", "p", "/target x-90a9"],
        err=err,
        profiles={"target": {"route": "zai,glm-5.3-flash[1m]"}},
    )
    assert "--route" in out
    assert "route skipped" not in err.getvalue()
    assert "applied route=zai,glm-5.3-flash[1m] (agents.profiles.target.route)" in err.getvalue()
    rows = _decision(journal)
    assert len(rows) == 1, rows
    assert ["route", "zai,glm-5.3-flash[1m]", "agents.profiles.target.route"] in [
        list(a) for a in rows[0]["applied"]
    ]


def test_one_decision_event_per_spawn_carries_every_axis(journal: Path) -> None:
    """AC20. Exactly one event, naming the spawn and all three axis groups."""
    import io as _io  # noqa: F401 - kept local for symmetry with siblings

    err = _io.StringIO()
    out = _inject(
        ["spawn", "--name", "decision-probe", "--provider", "zai", "--model", "m", "/target x"],
        err=err,
        profiles={"target": {"route": "zai,glm", "model": "opus"}},
    )
    rows = _decision(journal)
    assert len(rows) == 1, rows
    row = rows[0]
    assert row["name"] == "decision-probe"
    assert row["verb"] == "target"
    assert set(row["resolved"]) >= {
        "provider", "model", "effort", "substrate",
        "permission_mode", "route", "account", "pane_group",
    }
    assert any(a[0] == "model" for a in row["applied"]) is False  # explicit -m wins
    assert out[0] == "spawn"


def test_emit_failure_never_breaks_the_spawn(
    journal: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC21. A raising emit (or a raising state_dir) still returns the argv."""
    import io as _io  # noqa: F401 - kept local for symmetry with siblings

    def _boom(*a, **k):
        raise RuntimeError("journal gone")

    monkeypatch.setattr(agent_events, "emit", _boom)
    err = _io.StringIO()
    out = _inject(
        ["spawn", "--name", "p", "/target x"],
        err=err,
        profiles={"target": {"route": "zai,glm"}},
    )
    assert out[0] == "spawn"
    assert "--route" in out

    monkeypatch.setenv("FNO_TEST_HERMETIC", "1")
    monkeypatch.delattr(agent_events, "emit", raising=False)
    err = _io.StringIO()
    out2 = _inject(
        ["spawn", "--name", "p", "/target x"],
        err=err,
        profiles={"target": {"route": "zai,glm"}},
    )
    assert out2[0] == "spawn"
