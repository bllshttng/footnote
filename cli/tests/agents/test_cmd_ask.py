"""Tests for the wired-up `fno agents ask` Typer command — Task 1.3."""
from __future__ import annotations

from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.paths_testing import use_tmpdir
from tests.agents._fake_claude import configure_fake, install_fake_claude


@pytest.fixture
def runner() -> CliRunner:
    # Click 8.2+ separates stdout/stderr by default. result.stderr returns
    # the captured stderr; result.stdout returns stdout only.
    return CliRunner()


def test_cmd_ask_prints_short_id_only_on_success(
    tmp_path: Path, monkeypatch, runner: CliRunner
) -> None:
    """AC1-UI (post-bus-group1): unknown agent 'demo' -> exit 16 with unknown-agent message.
    The create-short-id contract moved to the spawn verb (Task 1.2)."""
    use_tmpdir(monkeypatch, tmp_path)
    bin_dir = tmp_path / "bin"
    install_fake_claude(bin_dir)
    monkeypatch.setenv("PATH", str(bin_dir))
    configure_fake(monkeypatch)

    from fno.agents.cli import agents_app
    from fno.agents.dispatch import UNKNOWN_AGENT_EXIT_CODE

    workdir = tmp_path / "work"
    workdir.mkdir()
    result = runner.invoke(
        agents_app,
        ["ask", "demo", "hi", "--harness", "claude", "--cwd", str(workdir)],
    )

    assert result.exit_code == UNKNOWN_AGENT_EXIT_CODE, result.output
    combined = (result.stderr or "") + (result.stdout or "")
    assert "unknown agent" in combined
    assert "spawn it first" in combined


def test_cmd_ask_short_flags_behave_like_long(
    tmp_path: Path, monkeypatch, runner: CliRunner
) -> None:
    """ab-3ff64151 AC1/AC4 (Python path): `ask -H <harness> -c <cwd>` parses correctly
    (both long and short flags reach dispatch_ask). Unknown agent -> exit 16 with both forms.
    Pins that the short flags are wired correctly even though the result is now exit 16."""
    use_tmpdir(monkeypatch, tmp_path)
    bin_dir = tmp_path / "bin"
    install_fake_claude(bin_dir)
    monkeypatch.setenv("PATH", str(bin_dir))
    configure_fake(monkeypatch)

    from fno.agents.cli import agents_app
    from fno.agents.dispatch import UNKNOWN_AGENT_EXIT_CODE

    workdir = tmp_path / "work"
    workdir.mkdir()
    result = runner.invoke(
        agents_app,
        ["ask", "demo", "hi", "-H", "claude", "-c", str(workdir)],
    )

    # Short flags parsed correctly; unknown agent -> exit 16 (not "unknown option")
    assert result.exit_code == UNKNOWN_AGENT_EXIT_CODE, (result.stdout or "") + (result.stderr or "")
    combined = (result.stderr or "") + (result.stdout or "")
    assert "unknown agent" in combined


def test_cmd_ask_provider_tombstone_teaches_and_exits_2(
    tmp_path: Path, monkeypatch, runner: CliRunner
) -> None:
    """x-bab1 AC3 (Python path): the retired --provider spelling is a hidden
    tombstone that exits 2 with the axis map; it never reaches dispatch."""
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app, ["ask", "demo", "hi", "--provider", "claude"]
    )
    assert result.exit_code == 2, (result.stdout or "") + (result.stderr or "")
    err = (result.stderr or "") + (result.stdout or "")
    assert "split at the axis rename" in err
    assert "--harness/-H" in err
    assert "0.4.0" in err


def test_cmd_ask_off_spawn_minus_p_is_unknown(
    tmp_path: Path, monkeypatch, runner: CliRunner
) -> None:
    """x-bab1 AC6 (Python path): -p is no longer ask's harness short; Typer
    rejects it as an unknown option (loud), never binding it to a harness."""
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app, ["ask", "demo", "hi", "-p", "claude"]
    )
    assert result.exit_code != 0
    err = (result.stderr or "") + (result.stdout or "")
    # Click/Typer's unknown-option surface names the flag it did not recognize.
    assert "-p" in err or "no such option" in err.lower()


def test_cmd_ask_surfaces_provider_required(
    tmp_path: Path, monkeypatch, runner: CliRunner
) -> None:
    """AC1-ERR (post-bus-group1): unknown agent with no --provider -> exit 16.
    The unknown-agent check precedes provider-required; exit 16 not exit 2."""
    use_tmpdir(monkeypatch, tmp_path)
    bin_dir = tmp_path / "bin"
    install_fake_claude(bin_dir)
    monkeypatch.setenv("PATH", str(bin_dir))

    from fno.agents.cli import agents_app
    from fno.agents.dispatch import UNKNOWN_AGENT_EXIT_CODE

    result = runner.invoke(agents_app, ["ask", "x", "hi"])

    assert result.exit_code == UNKNOWN_AGENT_EXIT_CODE
    err = (result.stderr or "") + (result.stdout or "")
    assert "unknown agent" in err
    assert "spawn it first" in err


def test_cmd_ask_surfaces_claude_not_on_path(
    tmp_path: Path, monkeypatch, runner: CliRunner
) -> None:
    """AC1-ERR (post-bus-group1): unknown agent -> exit 16 regardless of PATH.
    PATH check (exit 14) now lives in spawn verb; ask exits 16 for unknown names."""
    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.setenv("PATH", "/nonexistent")

    from fno.agents.cli import agents_app
    from fno.agents.dispatch import UNKNOWN_AGENT_EXIT_CODE

    result = runner.invoke(
        agents_app, ["ask", "y", "hi", "--harness", "claude"]
    )

    assert result.exit_code == UNKNOWN_AGENT_EXIT_CODE
    err = (result.stderr or "") + (result.stdout or "")
    assert "unknown agent" in err


def test_cmd_ask_surfaces_subprocess_stderr_verbatim(
    tmp_path: Path, monkeypatch, runner: CliRunner
) -> None:
    """AC1-FR (post-bus-group1): unknown agent -> exit 16 before reaching subprocess.
    The subprocess-error path (exit 1) is tested via _claude_create_path directly."""
    use_tmpdir(monkeypatch, tmp_path)
    bin_dir = tmp_path / "bin"
    install_fake_claude(bin_dir)
    monkeypatch.setenv("PATH", str(bin_dir))
    configure_fake(
        monkeypatch,
        exit_code=1,
        stderr="Error: not authenticated. Run claude /login\n",
    )

    from fno.agents.cli import agents_app
    from fno.agents.dispatch import UNKNOWN_AGENT_EXIT_CODE

    result = runner.invoke(
        agents_app, ["ask", "n", "hi", "--harness", "claude"]
    )

    # Unknown agent fires before subprocess; exit 16 not 1
    assert result.exit_code == UNKNOWN_AGENT_EXIT_CODE
    err = (result.stderr or "") + (result.stdout or "")
    assert "unknown agent" in err


# ---------------------------------------------------------------------------
# US2: --from-name flag + follow-up stdout contract
# ---------------------------------------------------------------------------


def test_cmd_ask_rejects_from_name_with_xml_unsafe_chars(
    tmp_path: Path, monkeypatch, runner: CliRunner
) -> None:
    """AC2-ERR: --from-name containing '\"<>&' → exit 2."""
    use_tmpdir(monkeypatch, tmp_path)
    bin_dir = tmp_path / "bin"
    install_fake_claude(bin_dir)
    monkeypatch.setenv("PATH", str(bin_dir))
    configure_fake(monkeypatch)

    from fno.agents.cli import agents_app

    for bad in ['bad"name', "ang<le", "and&", "ang>le"]:
        result = runner.invoke(
            agents_app,
            ["ask", "agent-x", "msg", "--harness", "claude",
             "--from-name", bad],
        )
        assert result.exit_code == 2, f"{bad!r} did not exit 2"
        err = (result.stderr or "") + (result.stdout or "")
        assert "XML-unsafe" in err


def test_cmd_ask_rejects_from_name_too_long(
    tmp_path: Path, monkeypatch, runner: CliRunner
) -> None:
    """AC2-ERR: --from-name >128 chars → exit 2."""
    use_tmpdir(monkeypatch, tmp_path)
    bin_dir = tmp_path / "bin"
    install_fake_claude(bin_dir)
    monkeypatch.setenv("PATH", str(bin_dir))
    configure_fake(monkeypatch)

    from fno.agents.cli import agents_app

    long_name = "a" * 129
    result = runner.invoke(
        agents_app,
        ["ask", "agent-x", "msg", "--harness", "claude",
         "--from-name", long_name],
    )
    assert result.exit_code == 2
    err = (result.stderr or "") + (result.stdout or "")
    assert "from-name must be <=128 chars" in err


def test_cmd_ask_from_name_default_is_fno(
    tmp_path: Path, monkeypatch
) -> None:
    """AC2-EDGE: default --from-name is "fno" — passthrough to dispatch."""
    use_tmpdir(monkeypatch, tmp_path)
    bin_dir = tmp_path / "bin"
    install_fake_claude(bin_dir)
    monkeypatch.setenv("PATH", str(bin_dir))
    configure_fake(monkeypatch)

    from fno.agents import cli as cli_mod

    captured: dict = {}

    def fake_dispatch_ask(**kwargs):  # type: ignore[no-untyped-def]
        captured.update(kwargs)
        from fno.agents.dispatch import DispatchAskResult
        return DispatchAskResult(kind="create", short_id="7c5dcf5d")

    monkeypatch.setattr(
        "fno.agents.dispatch.dispatch_ask", fake_dispatch_ask
    )

    runner = CliRunner()
    result = runner.invoke(
        cli_mod.agents_app,
        ["ask", "agent-x", "msg", "--harness", "claude"],
    )
    assert result.exit_code == 0, result.output
    assert captured["from_name"] == "fno"


def test_cmd_ask_from_name_custom_value_passes_through(
    tmp_path: Path, monkeypatch
) -> None:
    """AC2-EDGE: custom --from-name reaches dispatch_ask verbatim."""
    use_tmpdir(monkeypatch, tmp_path)
    bin_dir = tmp_path / "bin"
    install_fake_claude(bin_dir)
    monkeypatch.setenv("PATH", str(bin_dir))
    configure_fake(monkeypatch)

    from fno.agents import cli as cli_mod
    from fno.agents.dispatch import DispatchAskResult

    captured: dict = {}

    def fake_dispatch_ask(**kwargs):  # type: ignore[no-untyped-def]
        captured.update(kwargs)
        return DispatchAskResult(kind="create", short_id="7c5dcf5d")

    monkeypatch.setattr(
        "fno.agents.dispatch.dispatch_ask", fake_dispatch_ask
    )

    runner = CliRunner()
    result = runner.invoke(
        cli_mod.agents_app,
        ["ask", "agent-x", "msg", "--harness", "claude",
         "--from-name", "orchestrator-main"],
    )
    assert result.exit_code == 0, result.output
    assert captured["from_name"] == "orchestrator-main"


def test_cmd_ask_follow_up_stdout_is_reply_verbatim(
    tmp_path: Path, monkeypatch
) -> None:
    """AC2-HP / AC2-UI: follow-up stdout is the reply verbatim, no trailing
    newline added; stderr stays empty."""
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents import cli as cli_mod
    from fno.agents.dispatch import DispatchAskResult

    def fake_dispatch_ask(**kwargs):  # type: ignore[no-untyped-def]
        return DispatchAskResult(
            kind="followup",
            short_id="abc12345",
            reply="## Login.tsx changes\n\nAdded zod validation.\n",
        )

    monkeypatch.setattr(
        "fno.agents.dispatch.dispatch_ask", fake_dispatch_ask
    )

    runner = CliRunner()
    result = runner.invoke(
        cli_mod.agents_app, ["ask", "agent-x", "msg"]
    )
    assert result.exit_code == 0
    # Verbatim, including the recipient's own trailing newline. Abilities
    # adds NO extra newline.
    assert result.stdout == "## Login.tsx changes\n\nAdded zod validation.\n"
