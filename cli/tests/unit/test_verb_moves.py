"""Tests for the VERB_MOVES forwarding shim and the four deletes beside it.

A move is not a break: the old spelling stays registered for one release,
forwards in-process once the destination root exists, and prints one stderr
line naming the new spelling. Until the destination is minted the OLD
registration serves the call, announced rather than silent - except a hot
leaf in ``permanent_leaves``, which stays quiet forever. The four deleted verbs
(executor, posture, tokens, upgrade) are tombstones, not moves: the
capability is gone, so the refusal names where it went instead of forwarding.

The synthetic-app tests pin the forwarding mechanism end to end through the
real ``LazyTypeGroup.resolve_command``; the real-app tests pin the wiring on
the actual registry.
"""
from __future__ import annotations

import re

import click
import pytest
import typer
from typer.testing import CliRunner

from fno._lazy_group import make_lazy_group_cls
from fno.verb_moves import Move, VERB_MOVES, deprecation_line, forwarding_args, move_for

runner = CliRunner()


# ---------------------------------------------------------------------------
# deprecation_line: the policy, pure
# ---------------------------------------------------------------------------


def test_deprecated_entry_announces_the_bare_destination():
    move = VERB_MOVES["outstanding"]
    assert (
        deprecation_line("outstanding", [], move)
        == "fno outstanding is now fno inbox outstanding"
    )
    # Arguments of an entry without silent_leaves are values, not subcommands
    # to teach, so the line stays bare rather than echoing them.
    assert (
        deprecation_line("outstanding", ["list"], move)
        == "fno outstanding is now fno inbox outstanding"
    )


def test_silent_leaf_prints_nothing():
    move = VERB_MOVES["pr"]
    for leaf in ("status", "merge", "rebase"):
        assert deprecation_line("pr", [leaf, "993"], move) is None


def test_pr_permanent_leaves_are_explicit_lifetime_policy():
    move = VERB_MOVES["pr"]
    assert move.permanent_leaves == frozenset({"status", "merge", "rebase"})


def test_post_expiry_mode_forwards_only_permanent_leaves():
    expired = Move(
        kind="leaf-alias",
        to="do pr",
        permanent_leaves=frozenset({"status", "merge", "rebase"}),
    )
    assert forwarding_args(["status", "993"], expired) == ["do", "pr", "status", "993"]
    assert forwarding_args(["create"], expired) is None
    assert forwarding_args([], expired) is None


def test_cold_leaf_announces_the_leaf_qualified_destination():
    move = VERB_MOVES["pr"]
    assert (
        deprecation_line("pr", ["create"], move)
        == "fno pr create is now fno do pr create"
    )


def test_flag_first_argument_announces_the_bare_destination():
    # `fno pr --help` must announce (the shim was reached) without echoing a
    # flag into the teaching line.
    move = VERB_MOVES["pr"]
    assert deprecation_line("pr", ["--help"], move) == "fno pr is now fno do pr"


def test_alias_kind_never_prints():
    move = Move(kind="alias", to="do pr")
    assert deprecation_line("pr", [], move) is None
    assert deprecation_line("pr", ["create"], move) is None


# ---------------------------------------------------------------------------
# The forwarding mechanism, through the real LazyTypeGroup dispatch
# ---------------------------------------------------------------------------


def _app_with_old_registration(with_dest: bool) -> typer.Typer:
    """A scratch root app registering the moved spellings and (optionally)
    their destinations, on the same lazy-group class the real CLI uses."""
    app = typer.Typer(cls=make_lazy_group_cls({}))

    @app.callback()
    def _cb() -> None: ...

    old = typer.Typer(help="old outstanding")

    @old.command("list")
    def _old_list() -> None:
        typer.echo("OLD-PATH")

    app.add_typer(old, name="outstanding", hidden=True)

    if with_dest:
        inbox = typer.Typer(help="inbox")
        outstanding = typer.Typer(help="outstanding")

        @outstanding.command("list")
        def _inbox_outstanding_list() -> None:
            typer.echo("NEW-PATH")

        inbox.add_typer(outstanding, name="outstanding")
        app.add_typer(inbox, name="inbox", hidden=True)
    return app


def test_missing_destination_serves_old_registration_without_false_announcement():
    app = _app_with_old_registration(with_dest=False)
    result = runner.invoke(app, ["outstanding", "list"])
    assert result.exit_code == 0, result.output
    assert result.stdout == "OLD-PATH\n"
    assert "is now" not in (result.stderr or "")


def test_registered_destination_forwards_byte_identical():
    app = _app_with_old_registration(with_dest=True)
    direct = runner.invoke(app, ["inbox", "outstanding", "list"])
    forwarded = runner.invoke(app, ["outstanding", "list"])
    assert direct.exit_code == 0 and forwarded.exit_code == 0
    # stdout of the old spelling is byte-identical to the new path: callers
    # parse stdout, so the shim must not edit it.
    assert forwarded.stdout == direct.stdout == "NEW-PATH\n"
    err = forwarded.stderr or ""
    assert err.count("is now") == 1
    assert "fno inbox outstanding" in err
    assert "is now" not in (direct.stderr or "")


def test_hot_leaf_forwards_silently_through_the_destination():
    app = typer.Typer(cls=make_lazy_group_cls({}))

    @app.callback()
    def _cb() -> None: ...

    old_pr = typer.Typer(help="old pr")

    @old_pr.command("merge")
    def _old_merge(number: str) -> None:
        typer.echo(f"OLD-MERGE {number}")

    @old_pr.command("create")
    def _old_create() -> None:
        typer.echo("OLD-CREATE")

    app.add_typer(old_pr, name="pr", hidden=True)

    do = typer.Typer(help="do")
    pr = typer.Typer(help="pr")

    @pr.command("merge")
    def _new_merge(number: str) -> None:
        typer.echo(f"NEW-MERGE {number}")

    @pr.command("create")
    def _new_create() -> None:
        typer.echo("NEW-CREATE")

    do.add_typer(pr, name="pr")
    app.add_typer(do, name="do", hidden=True)

    merged = runner.invoke(app, ["pr", "merge", "993"])
    assert merged.exit_code == 0, merged.output
    assert merged.stdout == "NEW-MERGE 993\n"
    assert "is now" not in (merged.stderr or "")

    created = runner.invoke(app, ["pr", "create"])
    assert created.exit_code == 0, created.output
    assert created.stdout == "NEW-CREATE\n"
    err = created.stderr or ""
    assert "fno pr create is now fno do pr create" in err


def test_post_expiry_dispatch_forwards_hot_leaf_and_rejects_cold_leaf(monkeypatch):
    import fno.verb_moves as moves

    app = typer.Typer(cls=make_lazy_group_cls({}))

    @app.callback()
    def _cb() -> None: ...

    old_pr = typer.Typer()

    @old_pr.command("status")
    def _old_status() -> None:
        typer.echo("OLD")

    @old_pr.command("create")
    def _old_create() -> None:
        typer.echo("OLD")

    app.add_typer(old_pr, name="pr", hidden=True)
    do = typer.Typer()
    new_pr = typer.Typer()

    @new_pr.command("status")
    def _new_status() -> None:
        typer.echo("NEW")

    do.add_typer(new_pr, name="pr")
    app.add_typer(do, name="do", hidden=True)
    monkeypatch.setitem(
        moves.VERB_MOVES,
        "pr",
        Move(kind="leaf-alias", to="do pr", permanent_leaves=frozenset({"status"})),
    )

    hot = runner.invoke(app, ["pr", "status"])
    assert hot.exit_code == 0 and hot.stdout == "NEW\n"
    cold = runner.invoke(app, ["pr", "create"])
    assert cold.exit_code != 0
    assert "was removed; use fno do pr create" in cold.output


def test_post_expiry_rejects_cold_leaf_when_destination_is_missing(monkeypatch):
    import fno.verb_moves as moves

    app = typer.Typer(cls=make_lazy_group_cls({}))

    @app.callback()
    def _cb() -> None: ...

    old_pr = typer.Typer()

    @old_pr.command("create")
    def _old_create() -> None:
        typer.echo("OLD-CREATE-RAN")

    app.add_typer(old_pr, name="pr", hidden=True)
    monkeypatch.setitem(
        moves.VERB_MOVES,
        "pr",
        Move(kind="leaf-alias", to="do pr", permanent_leaves=frozenset({"status"})),
    )

    result = runner.invoke(app, ["pr", "create"])
    assert result.exit_code != 0
    assert "OLD-CREATE-RAN" not in result.output
    assert "was removed; use fno do pr create" in result.output


# ---------------------------------------------------------------------------
# The wiring on the real registry
# ---------------------------------------------------------------------------


def test_real_outstanding_serves_announced_until_inbox_mints():
    from fno.cli import app

    result = runner.invoke(app, ["outstanding", "--help"])
    assert result.exit_code == 0, result.output
    err = result.stderr or ""
    assert "fno outstanding is now fno inbox outstanding" in err
    assert err.count("is now") == 1


def test_real_pr_hot_leaf_help_stays_silent():
    from fno.cli import app

    result = runner.invoke(app, ["pr", "merge", "--help"])
    assert result.exit_code == 0, result.output
    assert "is now" not in (result.stderr or "")


def test_real_pr_cold_leaf_help_names_registered_destination():
    from fno.cli import app

    result = runner.invoke(app, ["pr", "verify", "--help"])
    assert result.exit_code == 0, result.output
    assert "fno pr verify is now fno do pr verify" in (result.stderr or "")


def test_every_moved_spelling_is_hidden():
    import typer.main

    from fno.cli import app

    assert VERB_MOVES, "the table must be populated for this test to mean anything"
    root = typer.main.get_command(app)
    ctx = click.Context(root)
    for name in VERB_MOVES:
        cmd = root.get_command(ctx, name)
        assert cmd is not None, f"moved spelling {name!r} must stay registered"
        assert getattr(cmd, "hidden", False), (
            f"moved spelling {name!r} must be hidden or menu-caps counts it as a root"
        )


def test_do_fold_move_table_matches_the_approved_work_order():
    expected = {
        "delivery": "do delivery",
        "loops": "do loops",
        "phase": "do phase",
        "plan": "do plan",
        "pr": "do pr",
        "pr-watch": "do pr watch",
        "research": "do research",
        "resume": "do resume",
        "review": "do review",
        "state": "do state",
        "stub-manifest": "do pr stub-manifest",
        "target": "do target",
        "think": "do think",
    }
    assert {name: VERB_MOVES[name].to for name in expected} == expected


def test_doctor_fold_move_table_matches_the_approved_work_order():
    expected = {
        "bundle": "doctor bundle",
        "codemap": "doctor codemap",
        "evals": "doctor evals",
        "event": "doctor event",
        "lint": "doctor lint",
        "observer": "doctor observer",
        "skill-diff": "doctor skill-diff",
        "status-fanout": "doctor event fanout",
    }
    assert {name: VERB_MOVES[name].to for name in expected} == expected


def test_agents_fold_move_table_matches_the_approved_work_order():
    expected = {
        "autonomy": "agents autonomy",
        "claim": "agents claim",
        "dispatch": "agents dispatch",
        "king": "agents king",
        "mcp": "agents mcp",
        "restart": "agents restart",
        "roles": "agents roles",
        "worker": "agents worker",
    }
    assert {name: VERB_MOVES[name].to for name in expected} == expected


def test_restored_root_spellings_are_canonical_not_moves():
    """mail, test, update are root-canonical again (2026-08-22 operator ruling).

    The root lazy registrations serve the call directly: no argv rewrite, no
    "is now" line. A move entry here would re-shadow the canonical spelling,
    so its absence is the pinned contract, not an omission to backfill.
    """
    from fno.cli import app

    for verb in ("mail", "test", "update"):
        assert verb not in VERB_MOVES, f"{verb} is root-canonical, not a move"
        result = runner.invoke(app, [verb, "--help"])
        assert result.exit_code == 0, (verb, result.output)
        assert "is now" not in (result.stderr or ""), verb


def test_restored_root_spellings_stay_hidden_so_menu_caps_hold():
    import typer.main

    from fno.cli import app

    root = typer.main.get_command(app)
    ctx = click.Context(root)
    for verb in ("mail", "test", "update"):
        cmd = root.get_command(ctx, verb)
        assert cmd is not None, f"restored spelling {verb!r} must stay registered"
        assert getattr(cmd, "hidden", False), (
            f"restored spelling {verb!r} must be hidden or menu-caps counts it as a root"
        )


def test_rest_fold_move_table_matches_the_approved_work_order():
    """Unit 6 (x-9d6c): backlog, config, whoami, workspace.

    ``done`` and ``runtime`` are deliberately ABSENT: they are merges whose
    old flag surface is not arg-compatible with the destination, so they use
    the decide-style module shim (a notice inside the old module) instead of
    an argv-rewriting VERB_MOVES entry.
    """
    expected = {
        "annotate": "backlog annotate",
        "carveout": "backlog carveout",
        "context": "whoami context",
        "cost": "whoami cost",
        "paths": "config paths",
        "plugins": "config plugins",
        "retro": "backlog retro",
        "route": "config route",
        "scoreboard": "whoami scoreboard",
        "setup": "config setup",
        "status": "whoami status",
        "worktree": "workspace worktree",
    }
    assert {name: VERB_MOVES[name].to for name in expected} == expected
    for merged in ("done", "runtime"):
        assert merged not in VERB_MOVES, (
            f"{merged} is a module-shim merge; a VERB_MOVES entry would "
            "rewrite argv onto an incompatible flag surface"
        )


def test_help_all_lists_moved_spellings_under_their_own_heading():
    from fno.cli import app

    result = runner.invoke(app, ["help", "--all"])
    assert result.exit_code == 0, result.output
    out = result.output
    assert "Moved spellings" in out
    assert "now fno inbox outstanding" in out
    assert "now fno do pr" in out
    commands_part = out.split("Moved spellings")[0]
    for name in VERB_MOVES:
        assert not re.search(rf"^  {re.escape(name)}\s", commands_part, re.MULTILINE), (
            f"moved spelling {name!r} must not render among the roots"
        )


def test_help_all_classifies_a_moved_eager_command_too(monkeypatch):
    """An eager inline command must partition like a lazy entry.

    A later wave moves `cost`, which is an eager @app.command rather than a
    LAZY_SUBCOMMANDS row. If the full menu classified only lazy entries,
    `cost` would render among the roots while the --help count line counted
    it as moved - the two surfaces disagreeing about one name.
    """
    from fno.cli import app
    from fno.verb_moves import Move

    monkeypatch.setitem(VERB_MOVES, "cost", Move(kind="deprecated", to="whoami cost"))
    result = runner.invoke(app, ["help", "--all"])
    assert result.exit_code == 0, result.output
    commands_part, moved_part = result.output.split("Moved spellings")
    assert not re.search(r"^  cost\s", commands_part, re.MULTILINE)
    assert re.search(r"^  cost\s+now fno whoami cost", moved_part, re.MULTILINE)


# ---------------------------------------------------------------------------
# The four deletes: tombstones that teach, not "No such command"
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    ("verb", "needle"),
    [
        ("executor", "skills/execute/scripts/resolve-executor.sh"),
        ("posture", "`fno config set`"),
        ("tokens", "`fno whoami context`"),
        ("upgrade", "`fno doctor update`"),
    ],
)
def test_deleted_verb_refuses_naming_its_replacement(verb: str, needle: str):
    from fno.cli import app

    result = runner.invoke(app, [verb])
    assert result.exit_code != 0
    combined = (result.output or "") + (result.stderr or "")
    assert f"`fno {verb}` was removed" in combined
    assert needle in combined, f"{verb} tombstone must name {needle}: {combined!r}"


def test_move_for_is_none_for_a_verb_that_still_exists():
    assert move_for("backlog") is None
    assert move_for("executor") is None, "a delete is a tombstone, never a move"
