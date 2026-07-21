"""FNO_NODE reaches bg and headless workers, not only mux panes (x-d157, AC6-HP).

``resolve_provenance`` has always built the FNO_NODE/FNO_SLUG/FNO_PLAN map, but
only the pane path called it. bg and headless build their child env from
``os.environ``, so the export has to land there for the origin-capture fallback
to fire anywhere outside a pane -- which is where the capture miss concentrates.
"""
from __future__ import annotations

from typing import Any, Dict

import pytest
from typer.testing import CliRunner

runner = CliRunner()


class _Gate:
    def release(self) -> None:
        pass


@pytest.fixture
def spawned_env(monkeypatch: pytest.MonkeyPatch) -> Dict[str, Any]:
    """Invoke cmd_spawn with the dispatch faked; capture the env the child sees."""
    from fno.agents import dispatch, spawn_gate

    monkeypatch.setattr(spawn_gate, "run_gate", lambda *a, **k: _Gate())
    monkeypatch.delenv("FNO_NODE", raising=False)
    monkeypatch.delenv("FNO_SLUG", raising=False)
    monkeypatch.delenv("FNO_PLAN", raising=False)

    seen: Dict[str, Any] = {}

    def fake_dispatch_spawn(**kwargs: Any) -> Any:
        import os

        # The provider builds the child env from os.environ at this point, so
        # reading it here is what the worker would actually inherit.
        seen["env"] = dict(os.environ)
        return dispatch.SpawnResult(
            kind="created", name=kwargs["name"], provider="claude", short_id="abcd1234"
        )

    monkeypatch.setattr("fno.agents.dispatch.dispatch_spawn", fake_dispatch_spawn)
    # Resolve without touching the real graph: the export wiring is the subject.
    monkeypatch.setattr(
        "fno.agents.mux_spawn.resolve_provenance",
        lambda node, slug=None, plan=None: (
            {"FNO_NODE": node, "FNO_SLUG": "a-slug"} if node else {}
        ),
    )
    return seen


@pytest.mark.parametrize("substrate", ["bg", "headless"])
def test_ac6_hp_node_driven_spawn_exports_fno_node(
    spawned_env: Dict[str, Any], substrate: str
) -> None:
    """AC6-HP: both non-pane substrates carry the bound node id to the worker.

    The two substrates share one code path by construction (the export sits
    after the pane arm returns, with no substrate branch), so this parametrize
    pins that they are NOT allowed to diverge rather than covering two branches.
    """
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app,
        ["spawn", "w1", "hi", "--provider", "claude",
         "--substrate", substrate, "--node", "x-aaaa"],
    )
    assert result.exit_code == 0, result.output
    assert spawned_env["env"]["FNO_NODE"] == "x-aaaa"
    assert spawned_env["env"]["FNO_SLUG"] == "a-slug"


def test_export_does_not_outlive_the_dispatch(spawned_env: Dict[str, Any]) -> None:
    """The export is scoped to the dispatch call, not left on the process.

    cmd_spawn is called in-process (tests, and any caller that spawns twice), so
    a surviving FNO_NODE would attribute the second spawn's worker to the first
    spawn's node.
    """
    import os

    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app,
        ["spawn", "w1", "hi", "--provider", "claude",
         "--substrate", "bg", "--node", "x-aaaa"],
    )
    assert result.exit_code == 0, result.output
    assert spawned_env["env"]["FNO_NODE"] == "x-aaaa"
    assert "FNO_NODE" not in os.environ


def test_ac6_hp_nodeless_spawn_exports_no_key(spawned_env: Dict[str, Any]) -> None:
    """AC6-HP: an ad-hoc spawn exports no FNO_NODE key, not an empty string.

    An empty-string origin would read as "captured, value blank" downstream,
    which is worse than absent: the fallback treats absent as "no signal".
    """
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app,
        ["spawn", "w1", "hi", "--provider", "claude", "--substrate", "bg"],
    )
    assert result.exit_code == 0, result.output
    assert "FNO_NODE" not in spawned_env["env"]


def test_the_real_resolver_yields_nothing_for_a_nodeless_spawn() -> None:
    """The fixture above stubs resolve_provenance, so pin the real one here too.

    Without this, the nodeless assertion only proves the stub behaves as
    written, not that production does.
    """
    from fno.agents.mux_spawn import resolve_provenance

    assert resolve_provenance(None) == {}
    assert "" not in resolve_provenance("x-aaaa", "a-slug", "").values()


def test_provenance_keys_are_set_or_cleared_as_a_group(
    spawned_env: Dict[str, Any], monkeypatch: pytest.MonkeyPatch
) -> None:
    """A parent's FNO_PLAN must not ride along with a child's FNO_NODE.

    A worker dispatching a child for a plan-less node would otherwise hand down
    its own plan path beside someone else's node id.
    """
    from fno.agents.cli import agents_app

    monkeypatch.setenv("FNO_PLAN", "/parent/plan.md")
    result = runner.invoke(
        agents_app,
        ["spawn", "w1", "hi", "--provider", "claude",
         "--substrate", "bg", "--node", "x-aaaa"],
    )
    assert result.exit_code == 0, result.output
    assert spawned_env["env"]["FNO_NODE"] == "x-aaaa"
    assert "FNO_PLAN" not in spawned_env["env"]
    # and the parent's own env is put back afterwards
    import os

    assert os.environ["FNO_PLAN"] == "/parent/plan.md"
