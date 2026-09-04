"""The attach verb asks the table before it refuses (x-a3e8).

``features.attach`` answers "can fno attach to a live session on this
harness"; the resume form field answers only "ships a native attach
subcommand". The refusal must read the reachability claim and name it,
never present a missing Python implementation as the harness's
inability - the defect that made a daemon-held codex thread and a
keeper-held agy session both read as unattachable.
"""
from __future__ import annotations

import subprocess

import pytest

from fno.agents.registry import AgentEntry
from fno.paths_testing import use_tmpdir


def _seed_row(monkeypatch, tmp_path, name: str, harness: str) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    entry = AgentEntry(
        name=name,
        harness=harness,
        harness_session_id="6f1a2b3c-0001-4000-8000-000000000001",
        cwd=str(tmp_path),
        log_path="",
        status="live",
    )
    monkeypatch.setattr(
        "fno.agents.registry.resolve_agent",
        lambda _name: type("R", (), {"entry": entry})(),
    )
    # No live mux server (exit 24): the daemon-kept portal is unreachable,
    # so every case here lands on the inline table read under test.
    from fno.agents import mux_spawn

    monkeypatch.setattr(
        mux_spawn,
        "_run_mux",
        lambda *a, **k: subprocess.CompletedProcess(
            args=[], returncode=24, stdout="", stderr="no live mux server"
        ),
    )


def _state(monkeypatch, state: str) -> None:
    import fno.agents.harness_map as harness_map

    monkeypatch.setattr(harness_map, "feature_claim", lambda name, key: state)


def test_an_unmeasured_attach_row_refuses_naming_key_state_and_probe(
    tmp_path, monkeypatch, capsys
) -> None:
    _seed_row(monkeypatch, tmp_path, "agyworker", "agy")
    _state(monkeypatch, "unmeasured")
    from fno.agents import dispatch

    result = dispatch.attach_agent("agyworker")
    assert result.exit_code == 13
    err = capsys.readouterr().err
    assert "features.attach" in err
    assert "'unmeasured'" in err
    assert "harness probe agy" in err
    assert "fno agents logs agyworker --follow" in err


def test_a_capable_attach_row_refuses_in_unwired_words(
    tmp_path, monkeypatch, capsys
) -> None:
    """`capable` and `absent` are different remedies, so they refuse in
    different words: unwired arm, not unable harness."""
    _seed_row(monkeypatch, tmp_path, "agyworker", "agy")
    _state(monkeypatch, "capable")
    from fno.agents import dispatch

    result = dispatch.attach_agent("agyworker")
    assert result.exit_code == 13
    err = capsys.readouterr().err
    assert "'capable'" in err
    assert "wired no attach arm" in err


def test_an_absent_attach_row_refuses_in_inability_words(
    tmp_path, monkeypatch, capsys
) -> None:
    _seed_row(monkeypatch, tmp_path, "agyworker", "agy")
    _state(monkeypatch, "absent")
    from fno.agents import dispatch

    result = dispatch.attach_agent("agyworker")
    assert result.exit_code == 13
    err = capsys.readouterr().err
    assert "'absent'" in err
    assert "no attachable session" in err


def test_a_native_keeper_harness_names_the_daemon_kept_lane(
    tmp_path, monkeypatch
) -> None:
    """Native on a keeper-held harness: the portal above is the wired arm,
    so a refusal here means the lane is down - exit 24 names how to bring
    it back, never the harness's inability."""
    _seed_row(monkeypatch, tmp_path, "agyworker", "agy")
    _state(monkeypatch, "native")
    from fno.agents import dispatch

    with pytest.raises(dispatch.DispatchAskError) as refused:
        dispatch.attach_agent("agyworker")
    assert refused.value.exit_code == 24
    assert "daemon-kept lane" in str(refused.value)
    assert "fno mux serve" in str(refused.value)


def test_the_packaged_rows_read_native_where_attach_is_wired() -> None:
    """The shipped table carries the measured claims: claude and codex own
    wired attach arms today (the native verb; resume plus the daemon
    endpoint), and the packaged copy cannot silently drop them."""
    from fno.agents.harness_map import feature_claim

    for harness in ("claude", "codex"):
        assert feature_claim(harness, "attach") == "native", harness
