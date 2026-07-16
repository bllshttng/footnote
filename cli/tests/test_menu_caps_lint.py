"""AC5-FR: the In-N-Out menu-cap ratchet (`fno lint menu-caps`, x-71b6).

The ratchet keeps the advertised command surface small: promoting a verb past
the cap fails lint with a message that names the offender and both remedies,
and passes again once the verb is hidden or the cap constant is raised.
"""
from __future__ import annotations

import typer
from typer.testing import CliRunner

import fno.lint_cli as L

runner = CliRunner()


def test_menu_caps_passes_on_the_shipped_registry():
    """The real curated menu is within caps (top-level <= 10)."""
    result = runner.invoke(L.app, ["menu-caps"])
    assert result.exit_code == 0, result.output
    assert "menu-caps: ok" in result.output


def test_menu_caps_fails_naming_offender_and_both_remedies(monkeypatch):
    """AC5-FR: over-cap fails, naming the offending verb and both remedies."""
    import typer.main

    from fno.cli import app as root_app

    advertised = len(L._visible_command_names(typer.main.get_command(root_app)))
    # Lower the cap below the shipped advertised set - the same branch a real
    # 11th advertised verb would hit.
    monkeypatch.setattr(L, "MENU_CAP_TOP_LEVEL", advertised - 1)
    result = runner.invoke(L.app, ["menu-caps"])
    assert result.exit_code == 1, result.output
    out = result.output
    # Names a concrete offending verb (the one over the cap).
    assert "over the cap:" in out
    # Both remedies, one of them naming the cap constant literally.
    assert "mark it hidden" in out
    assert "MENU_CAP_TOP_LEVEL" in out


def test_menu_caps_passes_once_remedy_applied(monkeypatch):
    """AC5-FR: raising the cap constant (remedy 2) clears the failure."""
    # Set the cap comfortably above the shipped set: the ratchet passes again.
    monkeypatch.setattr(L, "MENU_CAP_TOP_LEVEL", 50)
    result = runner.invoke(L.app, ["menu-caps"])
    assert result.exit_code == 0, result.output


def _make_group(n_visible: int, n_hidden: int) -> "typer.models.Any":  # type: ignore[name-defined]
    """A Click group with n_visible + n_hidden commands, for boundary tests."""
    import typer.main

    sub = typer.Typer(no_args_is_help=True)
    for i in range(n_visible):
        sub.command(f"v{i}")(lambda: None)
    for i in range(n_hidden):
        sub.command(f"h{i}", hidden=True)(lambda: None)
    return typer.main.get_command(sub)


def test_visible_command_names_ignores_hidden():
    """The counter reads only non-hidden commands (AC4-EDGE boundary)."""
    group = _make_group(n_visible=3, n_hidden=4)
    names = L._visible_command_names(group)
    assert sorted(names) == ["v0", "v1", "v2"]


def test_sub_app_cap_boundary_12_passes_13_fails():
    """AC-EDGE: a sub-app with exactly the cap passes; one over fails."""
    at_cap = L._visible_command_names(_make_group(L.MENU_CAP_SUB_APP, 0))
    over_cap = L._visible_command_names(_make_group(L.MENU_CAP_SUB_APP + 1, 0))
    assert len(at_cap) <= L.MENU_CAP_SUB_APP
    assert len(over_cap) > L.MENU_CAP_SUB_APP


def test_shipped_sub_apps_within_cap():
    """agents and backlog advertise <= MENU_CAP_SUB_APP verbs each."""
    import typer.main

    from fno.agents.cli import agents_app
    from fno.graph.cli import cli as backlog_app

    for name, app_obj in (("agents", agents_app), ("backlog", backlog_app)):
        group = typer.main.get_command(app_obj)
        visible = L._visible_command_names(group)
        assert len(visible) <= L.MENU_CAP_SUB_APP, (
            f"sub-app {name} advertises {len(visible)} verbs: {sorted(visible)}"
        )
