"""Portal placement is Rust-owned (the runtime that runs the spawn
validates and places its own flags). The Python lane keeps the pane
placement contract and must NOT advertise --portal: an option that exists
only in the help of the lane nobody runs is the defect this family pins
against. Split out of test_spawn_pane.py, which is over the file budget
and may only shrink.
"""

from pathlib import Path


def test_cmd_spawn_portal_is_not_advertised_on_the_python_lane(
    tmp_path: Path, monkeypatch
) -> None:
    # d-1d474a79: the placement flags are Rust-only. Under the Python
    # runtime the option does not exist, so a caller gets the parser's own
    # refusal - never a placement that silently never happens.
    from typer.testing import CliRunner

    import fno.agents.cli as agents_cli

    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    res = CliRunner().invoke(
        agents_cli.agents_app,
        ["spawn", "--name", "w2", "work", "--substrate", "thread", "--portal", "1"],
    )
    assert res.exit_code == 2, res.output
    # Typer renders the unknown-option error through rich: strip the ANSI
    # noise before reading the flag's name.
    import re

    plain = re.compile(r"\x1b\[[0-9;]*m").sub("", res.output)
    assert "--portal" in plain, plain


def test_cmd_spawn_placement_rejected_on_bg_substrate(tmp_path: Path, monkeypatch) -> None:
    # Pane geometry flags on a thread refuse with the pane-only contract:
    # the portal that would make them meaningful is placed by the Rust lane.
    from typer.testing import CliRunner

    import fno.agents.cli as agents_cli

    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    res = CliRunner().invoke(
        agents_cli.agents_app,
        ["spawn", "peer", "--harness", "claude", "--substrate", "bg", "-x", "left"],
    )
    assert res.exit_code == 2, res.output
    assert "--split/-x, --at, and --tab apply only to --substrate pane" in res.output


def test_cmd_spawn_placement_still_rejected_on_headless(tmp_path: Path, monkeypatch) -> None:
    # A one-shot hosts no pane at all, so the flags refuse with the pane-only
    # contract.
    from typer.testing import CliRunner

    import fno.agents.cli as agents_cli

    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    res = CliRunner().invoke(
        agents_cli.agents_app,
        ["spawn", "peer", "--harness", "claude", "--substrate", "headless", "-x", "left"],
    )
    assert res.exit_code == 2, res.output
    assert "--split/-x, --at, and --tab apply only to --substrate pane" in res.output


def test_cmd_spawn_bounded_placement_on_thread_is_a_one_step_refusal(
    tmp_path: Path, monkeypatch
) -> None:
    # Bounded placement can never combine with a thread, so the refusal must
    # not send the caller anywhere else first.
    from typer.testing import CliRunner

    import fno.agents.cli as agents_cli

    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    res = CliRunner().invoke(
        agents_cli.agents_app,
        ["spawn", "--name", "w2", "work", "--substrate", "thread", "--bounded-placement"],
    )
    assert res.exit_code == 2, res.output
    assert "--bounded-placement" in res.output


def test_cmd_spawn_tab_rejected_on_bg_substrate(tmp_path: Path, monkeypatch) -> None:
    from typer.testing import CliRunner

    import fno.agents.cli as agents_cli

    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    res = CliRunner().invoke(
        agents_cli.agents_app,
        ["spawn", "peer", "--harness", "claude", "--substrate", "bg", "--tab", "name:x"],
    )
    assert res.exit_code == 2, res.output
    assert "--split/-x, --at, and --tab apply only to --substrate pane" in res.output


def _stub_bg_dispatch(monkeypatch, name="w1"):
    """Stub the Python bg dispatch the way the default-lane tests need it.

    Mirrors test_dispatch_barrier._stub_headless_dispatch: the receipt path
    reads kind/name/short_id/provider off the result, nothing else.
    """
    from fno.agents import dispatch as dispatch_mod

    class R:
        pass

    R.kind = "created"
    R.name = name
    R.short_id = "abcd1234"
    R.provider = "claude"
    R.reply = None
    monkeypatch.setattr(dispatch_mod, "dispatch_spawn", lambda **kw: R())


def _stub_default_spawn_env(monkeypatch, tmp_path):
    """Route claims to tmp_path and stub the gate/provenance edges, so a bare
    spawn can run the whole cmd_spawn body to its receipt."""
    from fno.agents import mux_spawn, spawn_gate

    monkeypatch.setattr("fno.claims.io.claims_root_for", lambda key: tmp_path)
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path))
    monkeypatch.setattr(
        mux_spawn,
        "resolve_provenance",
        lambda n, s, p: {"FNO_NODE": "x-abcd"} if n else {},
    )

    class FakeGuard:
        def release(self):
            pass

    monkeypatch.setattr(
        spawn_gate, "run_gate", lambda name, substrate, **kw: FakeGuard()
    )
    monkeypatch.setattr("fno.agents.harness_map.thread_seatable", lambda h: True)


def test_bare_default_thread_spawn_opens_portal_zero_inside_a_mux(
    tmp_path, monkeypatch
) -> None:
    # The node's own acceptance shape: a BARE spawn inside a mux seats a
    # thread AND opens its default view. The view rides the same two-call
    # seam a user names by hand; the parser still declares no --portal.
    from typer.testing import CliRunner

    import fno.agents.cli as agents_cli
    from fno.agents import spawn_defaults

    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    monkeypatch.setenv("FNO_PANE", "%7")
    _stub_default_spawn_env(monkeypatch, tmp_path)
    _stub_bg_dispatch(monkeypatch)

    placed: list[tuple[str, int]] = []

    def fake_place(worker, portal=0):
        placed.append((worker, portal))
        return True

    monkeypatch.setattr(spawn_defaults, "place_default_view", fake_place)

    res = CliRunner().invoke(agents_cli.agents_app, ["spawn", "--name", "w1", "hi"])
    assert res.exit_code == 0, res.output
    assert placed == [("w1", 0)], placed


def test_bare_default_thread_spawn_survives_a_failed_view_placement(
    tmp_path, monkeypatch, capsys
) -> None:
    # A placement failure is a partial result: the worker keeps its live
    # receipt and exit 0; the stderr note names the manual reach.
    from typer.testing import CliRunner

    import fno.agents.cli as agents_cli
    from fno.agents import spawn_defaults

    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    monkeypatch.setenv("FNO_PANE", "%7")
    _stub_default_spawn_env(monkeypatch, tmp_path)
    _stub_bg_dispatch(monkeypatch)
    monkeypatch.setattr(spawn_defaults, "place_default_view", lambda worker: False)

    res = CliRunner().invoke(agents_cli.agents_app, ["spawn", "--name", "w1", "hi"])
    assert res.exit_code == 0, res.output
    assert '"name": "w1"' in res.output


def test_explicit_thread_substrate_names_its_own_view(tmp_path, monkeypatch) -> None:
    # An explicit --substrate thread is a named-lane spawn: the built-in
    # default's auto view does not fire (the caller names a portal, or runs
    # paneless and reaches for `fno mux thread` themselves).
    from typer.testing import CliRunner

    import fno.agents.cli as agents_cli
    from fno.agents import spawn_defaults

    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    monkeypatch.setenv("FNO_PANE", "%7")
    _stub_default_spawn_env(monkeypatch, tmp_path)
    _stub_bg_dispatch(monkeypatch)

    placed: list[str] = []

    def fake_place(worker):
        placed.append(worker)
        return True

    monkeypatch.setattr(spawn_defaults, "place_default_view", fake_place)

    res = CliRunner().invoke(
        agents_cli.agents_app,
        ["spawn", "--name", "w1", "--substrate", "thread", "hi"],
    )
    assert res.exit_code == 0, res.output
    assert placed == []


def test_place_default_view_runs_the_documented_two_call_seam(monkeypatch) -> None:
    # The helper's argv IS the documented reach: fno mux thread <name> --portal 0.
    import subprocess as subprocess_mod

    from fno.agents.spawn_defaults import place_default_view

    seen: list[list[str]] = []

    class Ok:
        returncode = 0
        stdout = ""
        stderr = ""

    def fake_run(argv, **kw):
        seen.append(list(argv))
        return Ok()

    monkeypatch.setattr(subprocess_mod, "run", fake_run)
    assert place_default_view("w1") is True
    assert seen == [["fno", "mux", "thread", "w1", "--portal", "0"]]


def test_place_default_view_failure_names_the_reach(
    monkeypatch, capsys
) -> None:
    import subprocess as subprocess_mod

    from fno.agents.spawn_defaults import place_default_view

    class Boom:
        returncode = 1
        stdout = ""
        stderr = "no live mux server\n"

    monkeypatch.setattr(subprocess_mod, "run", lambda argv, **kw: Boom())
    assert place_default_view("w1") is False
    assert "fno mux thread w1 --portal 0" in capsys.readouterr().err
