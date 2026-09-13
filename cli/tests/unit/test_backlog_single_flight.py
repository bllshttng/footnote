"""One-in-flight gate tests for `fno backlog advance` / `reconcile` (x-ef2c).

The measured shape: three concurrent `advance --epic` from three different
parents and three concurrent reconcile trees, the oldest twelve minutes, at
load 458 against a gate of 120. The contract under test: the SECOND
invocation for an in-flight scope reports `held`, runs nothing, and exits 0;
the work itself still completes; a scope never wedges shut.

Claim isolation mirrors test_advance.py: FNO_CLAIMS_ROOT / FNO_REPO_ROOT /
FNO_EVENTS_PATH pinned per test, so no gate state reaches a real store.
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path
from types import SimpleNamespace

import pytest
from typer.testing import CliRunner

from fno.backlog import advance as adv
from fno.backlog.single_flight import (
    Flight,
    acquire_flight,
    advance_flight_key,
    reconcile_flight_key,
)
from fno.claims.core import acquire_claim, claim_status
from fno.claims.io import claims_root_for
from fno.cli import app
from fno.rust_binary import find_dev_binary

runner = CliRunner()

requires_rust = pytest.mark.skipif(
    find_dev_binary() is None,
    reason="compiled fno-agents binary not present (build with `cargo build -p fno-agents`)",
)


@pytest.fixture
def iso(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """Pin claims + repo root + events under tmp_path, and point the gate's
    binary at THIS checkout's build (see test_advance.iso)."""
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path))
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.delenv("FNO_AUTO_CONTINUE", raising=False)
    events_path = tmp_path / ".fno" / "events.jsonl"
    monkeypatch.setenv("FNO_EVENTS_PATH", str(events_path))
    dev = find_dev_binary()
    if dev is None:
        pytest.skip("compiled fno-agents binary not present")
    monkeypatch.setenv("FNO_AGENTS_BIN", str(dev))
    return tmp_path


def _advance_result(decision: str = "disabled") -> SimpleNamespace:
    return SimpleNamespace(
        decision=decision, event="", reason="", node_id=None, short_id=None
    )


# ---------------------------------------------------------------------------
# AC1: the second invocation for an in-flight scope is held and runs nothing
# ---------------------------------------------------------------------------


def test_second_advance_reports_held_and_runs_nothing(iso, monkeypatch):
    """The x-ef2c VERIFY: fire the arm twice inside one run; the second
    reports held rather than starting a second run."""
    acquire_claim(advance_flight_key(None), "a-previous-run", ttl_ms=600_000)

    def _must_not_run(*_a, **_k):
        raise AssertionError("a held advance must not run the selection")

    monkeypatch.setattr(adv, "advance", _must_not_run)
    monkeypatch.setattr(adv, "advance_dependents", _must_not_run)

    result = runner.invoke(app, ["backlog", "advance", "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    assert payload["held"] is True
    assert payload["requests"] >= 1, "the held count is the positive marker"
    assert payload["holder"] == "a-previous-run"
    assert payload["decision"] == "held", "the --json receipt shape holds"


def test_second_epic_advance_reports_held(iso, monkeypatch):
    """Same contract for the daemon's converge entry (`--epic`), and a held
    receipt is never a retirement: the CLI exits 0 with `held: true`."""
    acquire_claim(advance_flight_key("x-epic-a"), "a-converge", ttl_ms=600_000)

    ran = []
    monkeypatch.setattr(
        "fno.backlog.advance.run_advance_epic",
        lambda *a, **k: ran.append(a),
    )

    result = runner.invoke(app, ["backlog", "advance", "--epic", "x-epic-a", "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    assert payload["held"] is True
    assert ran == []


def test_second_reconcile_reports_held(iso, monkeypatch):
    """The SessionStart/merge/groom arms all fire this verb; a second full
    sweep while one runs stands down with the held receipt."""
    acquire_claim(
        reconcile_flight_key(node=None, pr_number=None), "a-sweep", ttl_ms=600_000
    )

    ran = []
    monkeypatch.setattr(
        "fno.graph.cli._reconcile_once", lambda **k: ran.append(k)
    )

    result = runner.invoke(app, ["backlog", "reconcile", "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    assert payload["held"] is True
    assert ran == []


def test_refusal_fires_even_while_the_scope_is_held(iso, monkeypatch):
    """A bad invocation is refused before the gate: `--node` + `--pr-number`
    must still exit 2 while another sweep holds the scope."""
    acquire_claim(
        reconcile_flight_key(node=None, pr_number=None), "a-sweep", ttl_ms=600_000
    )
    result = runner.invoke(
        app, ["backlog", "reconcile", "--node", "ab-2222aaaa", "--pr-number", "7"]
    )
    assert result.exit_code == 2
    assert "held" not in result.output


# ---------------------------------------------------------------------------
# AC2: the work still completes - a held tick never drops it
# ---------------------------------------------------------------------------


def test_gate_releases_so_the_next_run_is_not_held(iso, monkeypatch):
    """Free scope: the run executes, the gate releases, and an immediate
    second run executes too (no stack, no wedge)."""
    calls = []
    monkeypatch.setattr(
        adv, "advance", lambda *a, **k: calls.append(1) or _advance_result()
    )

    first = runner.invoke(app, ["backlog", "advance", "--json"])
    second = runner.invoke(app, ["backlog", "advance", "--json"])
    assert first.exit_code == 0, first.output
    assert second.exit_code == 0, second.output
    assert json.loads(first.stdout).get("held") is None
    assert json.loads(second.stdout).get("held") is None
    assert calls == [1, 1]
    key = advance_flight_key(None)
    state = claim_status(key, root=claims_root_for(key))["state"]
    assert state == "free"


def test_gate_unavailable_fails_open(iso, monkeypatch):
    """No fno-agents binary (or one older than the verb): the gate is
    unavailable and the verb proceeds ungated, the pre-gate behavior."""
    monkeypatch.setenv("FNO_AGENTS_BIN", "/nonexistent/fno-agents")
    calls = []
    monkeypatch.setattr(
        adv, "advance", lambda *a, **k: calls.append(1) or _advance_result()
    )

    result = runner.invoke(app, ["backlog", "advance", "--json"])
    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout).get("held") is None
    assert calls == [1], "the run went ahead without the gate"


def test_a_dead_holder_does_not_wedge_the_scope(iso):
    """A holder whose process died is reclaimed on the pid probe, not the
    TTL: the next acquirer takes the scope and runs."""
    key = advance_flight_key(None)
    child = subprocess.Popen(["true"])
    child.wait()
    acquire_claim(key, "dead-run", ttl_ms=600_000, pid=child.pid)

    gate = acquire_flight(key, scope="advance")
    assert gate is not None and not gate.held, "a dead holder must never read as held"
    gate.release()


# ---------------------------------------------------------------------------
# Scopes: two copies of the SAME scope are the defect; distinct scopes are
# distinct work over the same store and never hold each other
# ---------------------------------------------------------------------------


def test_distinct_epics_hold_distinct_gates(iso):
    first = acquire_flight(advance_flight_key("x-epic-a"), scope="advance --epic a")
    other = acquire_flight(advance_flight_key("x-epic-b"), scope="advance --epic b")
    same = acquire_flight(advance_flight_key("x-epic-a"), scope="advance --epic a")
    assert first is not None and not first.held
    assert other is not None and not other.held
    assert same is not None and same.held
    first.release()
    other.release()


def test_pr_scoped_reconcile_does_not_queue_behind_a_full_sweep(iso):
    """A merge's own closure (`--pr-number`) is distinct work: it must not
    queue behind an unrelated twelve-minute graph sweep."""
    full = acquire_flight(
        reconcile_flight_key(node=None, pr_number=None), scope="reconcile"
    )
    pr = acquire_flight(
        reconcile_flight_key(node=None, pr_number=123), scope="reconcile pr 123"
    )
    same = acquire_flight(
        reconcile_flight_key(node=None, pr_number=None), scope="reconcile"
    )
    assert full is not None and not full.held
    assert pr is not None and not pr.held
    assert same is not None and same.held
    full.release()
    pr.release()


def test_dry_run_reconcile_is_never_gated(iso, monkeypatch):
    """--dry-run mutates nothing, so it stays readable while a real sweep
    runs: the operator inspects exactly when the graph is busiest."""
    acquire_claim(
        reconcile_flight_key(node=None, pr_number=None), "a-sweep", ttl_ms=600_000
    )
    ran = []
    monkeypatch.setattr("fno.graph.cli._reconcile_once", lambda **k: ran.append(k))
    result = runner.invoke(app, ["backlog", "reconcile", "--dry-run"])
    assert result.exit_code == 0, result.output
    assert ran, "a dry run must bypass the gate entirely"


# ---------------------------------------------------------------------------
# Held-stop: --stop is a control action and never queues behind the drain
# ---------------------------------------------------------------------------


def test_epic_stop_bypasses_the_gate(iso, monkeypatch):
    acquire_claim(advance_flight_key("x-epic-a"), "a-converge", ttl_ms=600_000)
    ran = []
    monkeypatch.setattr(
        "fno.backlog.advance.run_advance_epic",
        lambda *a, **k: ran.append(k),
    )
    result = runner.invoke(app, ["backlog", "advance", "--epic", "x-epic-a", "--stop"])
    assert result.exit_code == 0, result.output
    assert ran, "deactivating a mission must never be held by its own drain"
