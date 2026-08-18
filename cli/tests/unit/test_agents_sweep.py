"""The cadence-deadline backstop (AC8-*).

Run: cd cli && uv run pytest tests/unit/test_agents_sweep.py -q

Silence is a REPORT and acts never; a refusal is affirmative evidence and acts
immediately. These tests pin that asymmetry, because a component that acts on a
wrong silence reading loses work, and every measured failure in this fleet was
a case where the roster was wrong.
"""
from __future__ import annotations

from fno.agents import sweep as agents_sweep


class _Row:
    def __init__(self, name, harness="claude", cwd=""):
        self.name = name
        self.harness = harness
        self.cwd = cwd


def _truth(ages: dict):
    def fn(entry):
        age = ages.get(entry.name, "missing")
        if age == "missing":
            return {}
        return {
            "state": "working",
            "last_activity_age_s": age,
            "last_message": f"last turn from {entry.name}",
        }
    return fn


def _sweep(rows, ages, deadline=600, emit=None):
    return agents_sweep.run_sweep(
        emit=emit if emit is not None else (lambda t, d: None),
        deadline_s=deadline,
        registry_load=lambda: rows,
        truth_fn=_truth(ages),
    )


class TestSilencePredicate:
    def test_ac8_hp_a_quiet_worker_of_any_harness_is_a_finding(self):
        events = []
        rows, silent = _sweep(
            [_Row("w1", harness="codex"), _Row("w2")],
            {"w1": 1800, "w2": 30},
            emit=lambda t, d: events.append((t, dict(d))),
        )
        assert silent == 1
        assert [t for t, _ in events] == ["worker_silent"]
        payload = events[0][1]
        assert payload["handle"] == "w1"
        assert payload["harness"] == "codex"
        assert payload["age_s"] == 1800
        assert payload["deadline_s"] == 600

    def test_a_codex_successor_is_visible_here(self):
        # recovery's iter_candidates drops every non-claude row, so the sweep
        # that would catch a codex successor's own cap has to read the full
        # registry instead.
        rows, silent = _sweep([_Row("codex-successor", harness="codex")],
                              {"codex-successor": 900})
        assert silent == 1
        assert rows[0].harness == "codex"

    def test_ac8_edge_an_unknowable_age_emits_nothing(self):
        # resolve_session_truth returns None for age when a session keeps no
        # per-file transcript. Absence of evidence must not become a finding.
        events = []
        rows, silent = _sweep(
            [_Row("opencode-w", harness="opencode")], {"opencode-w": None},
            emit=lambda t, d: events.append((t, dict(d))),
        )
        assert silent == 0
        assert events == []
        assert rows[0].age_s is None
        assert rows[0].silent is False

    def test_a_missing_truth_dict_is_also_unknowable(self):
        rows, silent = _sweep([_Row("ghost")], {})
        assert silent == 0
        assert rows[0].age_s is None

    def test_exactly_at_the_deadline_is_not_yet_silent(self):
        _rows, silent = _sweep([_Row("w")], {"w": 600}, deadline=600)
        assert silent == 0
        _rows, silent = _sweep([_Row("w")], {"w": 601}, deadline=600)
        assert silent == 1

    def test_a_raising_truth_read_never_aborts_the_sweep(self):
        def _boom(entry):
            if entry.name == "bad":
                raise RuntimeError("unreadable transcript")
            return {"last_activity_age_s": 1800}

        rows, silent = agents_sweep.run_sweep(
            emit=lambda t, d: None, deadline_s=600,
            registry_load=lambda: [_Row("bad"), _Row("good")],
            truth_fn=_boom,
        )
        assert [r.handle for r in rows] == ["bad", "good"]
        assert silent == 1

    def test_an_unreadable_registry_reports_nothing(self):
        def _boom():
            raise RuntimeError("registry damaged")

        rows, silent = agents_sweep.run_sweep(
            emit=lambda t, d: None, registry_load=_boom, truth_fn=lambda e: {},
        )
        assert rows == [] and silent == 0


class TestNeverActs:
    def test_the_module_calls_no_lifecycle_verb(self):
        # A guard on the shape rather than on one call site: the constraint is
        # that NO path here stops, spawns, or unclaims, and a reader adding one
        # later should trip this rather than discover it in production.
        import inspect

        src = inspect.getsource(agents_sweep)
        # Call-shaped tokens, so the module's own prose about what failover
        # spawns does not trip its own guard.
        for forbidden in (
            "stop_agent(", "spawn_agent(", "write_registry(", "update_registry(",
            "_redispatch(", "release_claim(", "subprocess",
        ):
            assert forbidden not in src, forbidden


class TestJsonShape:
    def test_every_row_carries_deadline_s_including_healthy_ones(self):
        # done_probe 2 greps for "deadline_s". A key that appears only on the
        # unhappy path lets an empty answer pass the gate.
        rows, _silent = _sweep(
            [_Row("healthy"), _Row("quiet"), _Row("unknown")],
            {"healthy": 5, "quiet": 5000, "unknown": None},
        )
        assert len(rows) == 3
        for row in rows:
            assert "deadline_s" in row.as_dict()
            assert row.as_dict()["deadline_s"] == 600

    def test_the_row_is_json_serialisable(self):
        import json

        rows, _ = _sweep([_Row("w")], {"w": 42})
        assert json.loads(json.dumps([r.as_dict() for r in rows]))[0]["age_s"] == 42


class TestConfiguredDeadline:
    def test_a_bad_config_falls_back_to_the_builtin_window(self, monkeypatch):
        monkeypatch.setattr(
            agents_sweep, "load_settings", None, raising=False,
        )
        assert agents_sweep._configured_deadline() > 0
