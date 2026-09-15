"""The pane mint's launch record (schema v33).

``fno agents spawn --substrate pane`` stamps the row with the launched
argv minus the seed tokens: the one spelling of the launch axis a resume
replays. The harness pinning leg (grok records the uuid its argv
carries) is pinned beside it, because the pin is what makes that record
addressable.
"""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.paths_testing import use_tmpdir

from tests.agents.test_spawn_pane import _spawn


def _no_state_grant(monkeypatch: pytest.MonkeyPatch) -> None:
    """Exact-argv assertions need the machine-dependent writable-dir grant
    out of the argv (the same neutralizer test_spawn_pane's fixture uses)."""
    monkeypatch.setattr(
        "fno.agents.mux_spawn.worker_writable_dirs", lambda *a, **k: []
    )


def test_pane_mint_launch_record_drops_the_seed_keeps_the_tail(
    tmp_path: Path, monkeypatch
) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    _no_state_grant(monkeypatch)
    _result, runner = _spawn(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry

    run_call = runner.calls[0]
    tail = run_call[run_call.index("--") + 1 :]
    provider_argv = tail[tail.index("claude") :]
    seed = provider_argv[provider_argv.index("--") + 1]

    (row,) = load_registry()
    assert row.launch is not None, "the pane mint stamps the launch record"
    assert row.launch["argv"] == [t for t in provider_argv if t != seed]
    assert seed not in row.launch["argv"]


def test_grok_pane_row_records_the_pinned_uuid(
    tmp_path: Path, monkeypatch
) -> None:
    """The pinned uuid rides the argv once (``--session-id <uuid>``) and is
    the row's harness_session_id - the launch record and the row agree."""
    use_tmpdir(monkeypatch, tmp_path)
    _no_state_grant(monkeypatch)
    result, runner = _spawn(monkeypatch, tmp_path, provider="grok")
    from fno.agents.registry import load_registry

    run_call = runner.calls[0]
    tail = run_call[run_call.index("--") + 1 :]
    pinned = tail[tail.index("--session-id") + 1]
    assert tail.count(pinned) == 1, "the uuid rides the argv exactly once"

    (row,) = load_registry()
    assert row.harness_session_id == pinned
    assert result.session_uuid == pinned
    assert row.launch is not None
