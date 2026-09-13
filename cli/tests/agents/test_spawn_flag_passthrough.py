"""Spawn flag passthrough: every harness flag rides the substrate its
spawn resolves, and a flag the harness's thread lane cannot carry demotes
the spawn to the pane instead of refusing. The carrier facts live in the
capability contract's ``[harness.<name>.thread]`` rows; one predicate
(``harness_map.thread_uncarried``) decides, and both front doors (the
Python CLI and the spawn seam) call it.
"""

from pathlib import Path

import pytest
import typer
from typer.testing import CliRunner

from fno.paths_testing import use_tmpdir


def _setup_tmp_home(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    home = tmp_path / "home"
    home.mkdir(exist_ok=True)
    monkeypatch.setenv("HOME", str(home))
    for k in ("FNO_AGENT_SELF", "FNO_AGENT_HARNESS", "FNO_AGENT_SESSION"):
        monkeypatch.delenv(k, raising=False)


def _run(monkeypatch: pytest.MonkeyPatch, argv: list[str]):
    import fno.agents.cli as agents_cli

    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    return CliRunner().invoke(agents_cli.agents_app, argv)


def _capture_pane_dispatch(monkeypatch: pytest.MonkeyPatch) -> dict:
    """Record a demoted pane launch and stop there, so no mux session is
    touched."""
    import fno.agents.mux_spawn as mux_spawn

    calls: dict = {}

    def recorder(**kwargs):
        calls.update(kwargs)
        raise typer.Exit(code=0)

    monkeypatch.setattr(mux_spawn, "dispatch_spawn_bounded_pane", recorder)
    return calls


def _capture_thread_dispatch(monkeypatch: pytest.MonkeyPatch) -> dict:
    """Record a dispatch_spawn call (the thread/headless arm) and stop
    there."""
    import fno.agents.dispatch as dispatch

    calls: dict = {}

    def recorder(**kwargs):
        calls.update(kwargs)
        raise typer.Exit(code=0)

    monkeypatch.setattr(dispatch, "dispatch_spawn", recorder)
    return calls


# ---------------------------------------------------------------------------
# The predicate
# ---------------------------------------------------------------------------


def test_thread_uncarried_reads_the_thread_rows() -> None:
    from fno.agents.harness_map import thread_uncarried

    # codex thread carries the five typed axes it has a native form for.
    assert thread_uncarried("codex", {"effort": "high"}, None) is None
    assert thread_uncarried("codex", {"permission_mode": "yolo"}, None) is None
    assert thread_uncarried("codex", {"launch_role": "reviewer"}, None) == "--role"
    assert thread_uncarried("codex", {"agent": "reviewer"}, None) == "--agent"
    # ...and the three fenced spellings it maps.
    assert thread_uncarried("codex", {}, ["-c", "key=1"]) is None
    assert thread_uncarried("codex", {}, ["--config", "key=1"]) is None
    assert thread_uncarried("codex", {}, ["--add-dir", "/tmp/x"]) is None
    assert thread_uncarried("codex", {}, ["-p", "profile"]) == "-p"
    assert thread_uncarried("codex", {}, ["--profile", "p"]) == "--profile"

    # claude thread carries every axis and every fenced token.
    assert thread_uncarried("claude", {"agent": "a", "effort": "high"}, ["--anything"]) is None

    # opencode thread carries model only.
    assert thread_uncarried("opencode", {"model": "m"}, None) is None
    assert thread_uncarried("opencode", {"effort": "high"}, None) == "--effort"
    assert thread_uncarried("opencode", {}, ["--foo"]) == "--foo"


def test_thread_uncarried_reads_the_keeper_rows() -> None:
    from fno.agents.harness_map import thread_uncarried

    # A keeper-lane harness has no [thread] row: the keeper row answers.
    assert thread_uncarried("agy", {"effort": "high"}, None) is None
    assert thread_uncarried("agy", {"launch_role": "r"}, None) == "--role"
    assert thread_uncarried("grok", {"add_dir": "/tmp/d"}, None) == "--add-dir"
    assert thread_uncarried("grok", {"effort": "high"}, None) is None
    # Keeper lanes append every fenced token to the launch argv.
    assert thread_uncarried("pi", {}, ["--whatever", "x"]) is None
    assert thread_uncarried("cursor-agent", {"agent": "a"}, None) == "--agent"


# ---------------------------------------------------------------------------
# The resolver demotes instead of refusing (AC4)
# ---------------------------------------------------------------------------


def test_codex_role_demotes_to_pane(monkeypatch: pytest.MonkeyPatch) -> None:
    sent = _capture_pane_dispatch(monkeypatch)
    res = _run(monkeypatch, ["spawn", "work", "--harness", "codex", "--role", "reviewer"])
    assert sent.get("provider") == "codex", res.output
    assert "no carrier for --role" in res.output, res.output
    assert "is not supported on the codex thread lane" not in res.output


def test_codex_fenced_profile_demotes_to_pane(monkeypatch: pytest.MonkeyPatch) -> None:
    sent = _capture_pane_dispatch(monkeypatch)
    res = _run(monkeypatch, ["spawn", "work", "--harness", "codex", "--", "-p", "profile"])
    assert sent.get("provider") == "codex", res.output
    assert "no carrier for -p" in res.output, res.output


def test_grok_add_dir_demotes_to_pane(monkeypatch: pytest.MonkeyPatch) -> None:
    sent = _capture_pane_dispatch(monkeypatch)
    res = _run(monkeypatch, ["spawn", "work", "--harness", "grok", "--add-dir", "/tmp/d"])
    assert sent.get("provider") == "grok", res.output
    assert "no carrier for --add-dir" in res.output, res.output


def test_explicit_thread_meeting_an_uncarried_flag_demotes_loudly(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    sent = _capture_pane_dispatch(monkeypatch)
    res = _run(
        monkeypatch,
        ["spawn", "work", "--harness", "codex", "--substrate", "thread", "--role", "reviewer"],
    )
    assert sent.get("provider") == "codex", res.output
    assert (
        "fno agents spawn: substrate: pane (the codex thread lane has no carrier for --role)"
        in res.output
    ), res.output


def test_codex_thread_with_carried_flags_stays_thread(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    sent = _capture_thread_dispatch(monkeypatch)
    pane = _capture_pane_dispatch(monkeypatch)
    res = _run(
        monkeypatch,
        ["spawn", "work", "--harness", "codex", "--substrate", "thread", "--effort", "high"],
    )
    assert sent.get("harness") == "codex", res.output
    assert sent.get("effort") == "high", res.output
    assert sent.get("headless") is False, res.output
    assert not pane, "a carried flag must not demote to pane"
    assert "no carrier" not in res.output, res.output


def test_legacy_flag_shaped_seed_fence_rides_the_thread_as_the_prompt(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # One fenced token with NO message before the fence is the seed idiom,
    # and the seam promotes it to the MESSAGE before the resolver runs: it
    # rides the thread lane as the prompt, never read as passthrough.
    sent = _capture_thread_dispatch(monkeypatch)
    pane = _capture_pane_dispatch(monkeypatch)
    res = _run(monkeypatch, ["spawn", "--harness", "codex", "--", "run the tests"])
    assert sent.get("harness") == "codex", res.output
    assert sent.get("message") == "run the tests", res.output
    assert not pane, res.output


# ---------------------------------------------------------------------------
# Flags no substrate carries refuse by name, naming the -- fence (AC4-EDGE)
# ---------------------------------------------------------------------------


def test_codex_agent_refuses_on_the_pane_naming_the_fence() -> None:
    # --agent has no codex spelling on ANY substrate; the pane lane's own
    # token builder refuses, and the text names the -- fence as the way to
    # pass the harness's own flag.
    import pytest as _pytest

    from fno.agents.dispatch import DispatchAskError
    from fno.agents.mux_spawn import tier3_pane_tokens

    with _pytest.raises(DispatchAskError) as excinfo:
        tier3_pane_tokens("codex", agent="reviewer")
    assert "--agent" in str(excinfo.value)
    assert "fence" in str(excinfo.value)


def test_codex_agent_refuses_on_headless_naming_the_fence(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    res = _run(
        monkeypatch,
        ["spawn", "work", "--harness", "codex", "--substrate", "headless", "--agent", "reviewer"],
    )
    assert res.exit_code == 2, res.output
    assert "--agent" in res.output, res.output
    assert "fence" in res.output, res.output


# ---------------------------------------------------------------------------
# -c stays --cwd; codex config rides the fence (AC8)
# ---------------------------------------------------------------------------


def test_dash_c_config_value_stops_before_launch(monkeypatch: pytest.MonkeyPatch) -> None:
    res = _run(monkeypatch, ["spawn", "work", "-c", "model_reasoning_effort=high"])
    assert res.exit_code == 2, res.output
    assert "--cwd" in res.output, res.output
    assert "-- -c key=value" in res.output, res.output
    assert "no worker launched" in res.output, res.output


# ---------------------------------------------------------------------------
# The codex thread lane forwards what it carries (AC1)
# ---------------------------------------------------------------------------


def _capture_codex_thread_spawn(monkeypatch: pytest.MonkeyPatch) -> dict:
    import fno.agents.dispatch as dispatch

    sent: dict = {}

    def fake_codex_thread_spawn(**kwargs):
        sent.update(kwargs)
        return "01a0aaaa-bbbb-cccc-dddd-eeeeffff0000"

    monkeypatch.setattr(dispatch, "_codex_thread_spawn", fake_codex_thread_spawn)
    return sent


def test_operator_template_carries_effort_to_the_codex_thread_lane(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    # The operator's template: effort, yolo and a model on a spawn with no
    # substrate. It resolves thread and DELEGATES with the flags intact; the
    # old refusal ("--effort is not supported on the codex thread lane") is
    # retired.
    _setup_tmp_home(tmp_path, monkeypatch)
    sent = _capture_codex_thread_spawn(monkeypatch)
    res = _run(
        monkeypatch,
        [
            "spawn", "do the work", "--name", "w1",
            "--harness", "codex", "--model", "gpt-5.6-sol",
            "--effort", "high", "--yolo",
        ],
    )
    assert sent.get("effort") == "high", res.output
    assert sent.get("model") == "gpt-5.6-sol", res.output
    assert sent.get("yolo") is True, res.output
    assert "is not supported on the codex thread lane" not in res.output, res.output
    assert res.exit_code == 0, res.output


def test_explicit_thread_effort_carries_the_same_way(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    _setup_tmp_home(tmp_path, monkeypatch)
    sent = _capture_codex_thread_spawn(monkeypatch)
    res = _run(
        monkeypatch,
        [
            "spawn", "do the work", "--name", "w1", "--harness", "codex",
            "--substrate", "thread", "--effort", "high",
        ],
    )
    assert sent.get("effort") == "high", res.output
    assert res.exit_code == 0, res.output


def test_gemini_effort_keeps_its_refusal(monkeypatch: pytest.MonkeyPatch) -> None:
    # gemini has no reasoning-effort surface on any lane: that refusal is a
    # property of the harness, not of the substrate, and it stays.
    res = _run(monkeypatch, ["spawn", "work", "--harness", "gemini", "--effort", "high"])
    assert res.exit_code == 2, res.output
    assert "reasoning-effort" in res.output, res.output


# ---------------------------------------------------------------------------
# The claude bg lane appends fenced tokens to its argv (AC5)
# ---------------------------------------------------------------------------


def test_claude_bg_argv_carries_fenced_tokens(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    import fno.agents.harnesses.claude as claude_mod

    _setup_tmp_home(tmp_path, monkeypatch)
    captured: dict = {}

    class _FakeResult:
        returncode = 0
        stdout = "deadbeef\n"
        stderr = ""

    def fake_run(argv, **kwargs):
        captured["argv"] = list(argv)
        return _FakeResult()

    monkeypatch.setattr(claude_mod, "_subprocess_run", fake_run)
    res = _run(
        monkeypatch,
        [
            "spawn", "work", "--name", "w2", "--harness", "claude",
            "--substrate", "thread", "--", "--verbose",
        ],
    )
    argv = captured.get("argv") or []
    assert "--verbose" in argv, res.output
    assert "no carrier" not in res.output, res.output


def test_claude_bg_refuses_a_duplicate_fenced_flag(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    # A token naming a flag fno itself emits is two sources for one value:
    # the pane lane's named refusal, on the bg lane too.
    import fno.agents.harnesses.claude as claude_mod

    _setup_tmp_home(tmp_path, monkeypatch)
    captured: dict = {}

    def fake_run(argv, **kwargs):
        captured["argv"] = list(argv)
        raise AssertionError("no launch expected")

    monkeypatch.setattr(claude_mod, "_subprocess_run", fake_run)
    res = _run(
        monkeypatch,
        [
            "spawn", "work", "--name", "w3", "--harness", "claude",
            "--substrate", "thread", "--add-dir", "/tmp/d", "--", "--add-dir", "/tmp/e",
        ],
    )
    assert res.exit_code == 2, res.output
    assert "both sides" in res.output, res.output
    assert not captured, "no launch expected"


# ---------------------------------------------------------------------------
# The headless one-shot lanes append fenced tokens (AC6)
# ---------------------------------------------------------------------------


def test_claude_headless_argv_carries_fenced_tokens(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    import fno.agents.harnesses.claude as claude_mod

    _setup_tmp_home(tmp_path, monkeypatch)
    captured: dict = {}

    class _FakeResult:
        returncode = 0
        stdout = "the reply\n"
        stderr = ""

    def fake_run(argv, **kwargs):
        captured["argv"] = list(argv)
        return _FakeResult()

    monkeypatch.setattr(claude_mod, "_subprocess_run", fake_run)
    claude_mod.headless_create(message="work", cwd=tmp_path, passthrough=["--verbose"])
    argv = captured.get("argv") or []
    assert "--verbose" in argv, argv
    fence = argv.index("--")
    assert argv.index("--verbose") < fence, "tokens ride before the prompt fence"


def test_dash_c_existing_directory_works_as_before(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    sent = _capture_thread_dispatch(monkeypatch)
    res = _run(monkeypatch, ["spawn", "work", "-c", str(tmp_path)])
    assert sent.get("cwd") == tmp_path, res.output
    assert "no worker launched" not in res.output, res.output
    assert "-- -c key=value" not in res.output, res.output
