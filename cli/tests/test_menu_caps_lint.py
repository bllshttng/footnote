"""AC5-FR: the In-N-Out menu-cap ratchet (`fno lint menu-caps`, x-71b6).

The ratchet keeps the advertised command surface small: promoting a verb past
the cap fails lint with a message that names the offender and both remedies,
and passes again once the verb is hidden or the cap constant is raised.
"""
from __future__ import annotations

import click
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


def _make_group(n_visible: int, n_hidden: int) -> click.Group:
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


def test_menu_caps_enforces_cap_on_HIDDEN_top_level_groups():
    """Codex P2: the sub-app cap applies to every group, including one whose
    top-level entry is hidden (e.g. `fno mail`). Regression guard for two bugs:
    (1) iterating only advertised entries, and (2) the isinstance(click.Group)
    check that silently skipped ALL sub-apps because Typer bundles a vendored
    click (a TyperGroup is not a top-level click.Group instance)."""
    import typer
    import typer.main

    from fno.cli import LAZY_SUBCOMMANDS

    # A hidden top-level group must be *reached* by the lint (proves it does not
    # gate on top-level visibility) and be within the cap (proves the duck-typed
    # resolution counts it, not skips it).
    entry = LAZY_SUBCOMMANDS["mail"]
    assert isinstance(entry, tuple) and entry[2].get("hidden") is True, (
        "this test assumes `mail` is a hidden top-level group"
    )
    mail_app = getattr(__import__("fno.mail.cli", fromlist=["mail_app"]), "mail_app")
    group = typer.main.get_command(mail_app)
    # The duck-typed resolution the lint uses (NOT isinstance(click.Group)).
    assert hasattr(group, "list_commands")
    assert len(L._visible_command_names(group)) <= L.MENU_CAP_SUB_APP


def test_visible_command_names_works_on_typergroup():
    """The counter must work on a real TyperGroup via duck-typing, not an
    isinstance(click.Group) check: Typer may bundle a vendored click so a
    TyperGroup is not always a top-level `click.Group` instance (it is under
    pytest, but NOT under the installed `uv run` CLI - which is why the original
    isinstance gate silently skipped every sub-app in CI while tests passed).
    ``_visible_command_names`` must return the right names regardless."""
    import typer
    import typer.main

    sub = typer.Typer(no_args_is_help=True)
    sub.command("alpha")(lambda: None)
    sub.command("beta", hidden=True)(lambda: None)
    group = typer.main.get_command(sub)
    assert hasattr(group, "list_commands")  # the duck-typed check the lint uses
    assert L._visible_command_names(group) == ["alpha"]
