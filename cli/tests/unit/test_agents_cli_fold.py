from __future__ import annotations

import click
import json
import os
from pathlib import Path
import subprocess
import sys
import typer.main
from typer.testing import CliRunner


runner = CliRunner()
REPO_ROOT = Path(__file__).resolve().parents[3]


def _commands(app) -> set[str]:
    command = typer.main.get_command(app)
    assert isinstance(command, click.Group)
    return set(command.list_commands(click.Context(command)))


def test_agents_registers_the_nine_folded_roots() -> None:
    from fno.agents.cli import agents_app

    assert {
        "autonomy",
        "claim",
        "dispatch",
        "king",
        "mail",
        "mcp",
        "restart",
        "roles",
        "worker",
    } <= _commands(agents_app)


def test_agents_king_excludes_board_while_inbox_keeps_it() -> None:
    from fno.inbox.cli import inbox_app
    from fno.king.cli import agents_king_app

    assert "board" not in _commands(agents_king_app)
    assert "board" in _commands(inbox_app)


def test_old_agents_fold_spellings_forward_and_teach() -> None:
    from fno.cli import app

    for old, destination in (
        ("autonomy", "agents autonomy"),
        ("claim", "agents claim"),
        ("dispatch", "agents dispatch"),
        ("king", "agents king"),
        ("mail", "agents mail"),
        ("mcp", "agents mcp"),
        ("restart", "agents restart"),
        ("roles", "agents roles"),
        ("worker", "agents worker"),
    ):
        result = runner.invoke(app, [old, "--help"])
        assert result.exit_code == 0, (old, result.output)
        assert f"fno {old} is now fno {destination}" in (result.stderr or "")


def test_old_king_board_forwards_to_inbox_not_agents() -> None:
    from fno.cli import app

    result = runner.invoke(app, ["king", "board", "--help"])
    assert result.exit_code == 0, result.output
    assert "fno king board is now fno inbox board" in (result.stderr or "")


def test_rust_shellouts_use_the_folded_mcp_and_board_paths() -> None:
    daemon = (REPO_ROOT / "crates/fno-agents/src/daemon.rs").read_text()
    loopcheck = (REPO_ROOT / "crates/fno-agents/src/loopcheck.rs").read_text()

    assert '.args(["agents", "mcp", "send", "--session-id", channel_id])' in daemon
    assert '.args(["inbox", "board", "--json"])' in loopcheck


def test_growth_launch_uses_the_folded_roles_path() -> None:
    skill = (REPO_ROOT / "skills/growth-launch/SKILL.md").read_text()

    for action in ("context", "show", "resolve"):
        assert f"fno agents roles {action}" in skill


def test_folded_groups_do_not_consume_agents_leaf_cap() -> None:
    from fno.cli import COLLAPSE_KEEP

    assert COLLAPSE_KEEP["agents"].isdisjoint(
        {"autonomy", "claim", "dispatch", "king", "mail", "mcp", "roles", "worker"}
    )


def test_importing_agents_cli_does_not_import_folded_subgroups() -> None:
    modules = {
        "fno.autonomy_cli",
        "fno.claims.cli",
        "fno.dispatch",
        "fno.king.cli",
        "fno.mail.cli",
        "fno.mcp.cli",
        "fno.restart",
        "fno.roles.cli",
        "fno.worker.cli",
    }
    script = (
        "import json, sys; import fno.agents.cli; "
        f"print(json.dumps(sorted(set(sys.modules) & {modules!r})))"
    )
    env = {**os.environ, "PYTHONPATH": str(REPO_ROOT / "cli/src")}
    result = subprocess.run(
        [sys.executable, "-c", script],
        cwd=REPO_ROOT,
        env=env,
        capture_output=True,
        text=True,
        check=True,
    )

    assert json.loads(result.stdout) == []


def test_internal_variable_and_rust_callers_use_folded_paths() -> None:
    king_loop = (REPO_ROOT / "crates/fno-agents/src/loop_king.rs").read_text()
    eval_sweep = (REPO_ROOT / "scripts/lib/eval-sweep-throttle.sh").read_text()

    assert '"agents",\n            "king",\n            "escalate"' in king_loop
    assert '"$fno_cmd" agents claim acquire' in eval_sweep
    assert '"$fno_cmd" agents claim release' in eval_sweep
