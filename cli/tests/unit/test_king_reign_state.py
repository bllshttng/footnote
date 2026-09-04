"""``reign_state``: one reader for who is reigning, in what shape, and is it live.

The four cases the plan pins (agreeing, split, no manifest, unreadable
registry) plus the shape rewrite. Every assertion on an unknown names the
reason: a clean ``False`` where the instrument could not read is the
absence-lie this reader exists to prevent, so the tests refuse to let one in.
"""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.paths_testing import use_tmpdir


def _entry(name: str, **kw):
    from fno.agents.registry import AgentEntry

    harness = kw.pop("harness", "claude")
    kw.setdefault("harness_session_id", f"{name}-session")
    return AgentEntry(name=name, cwd="/w", log_path="", harness=harness, **kw)


def _write_manifest(tmp_path: Path, scope: str, session: str, shape: str = "pass") -> Path:
    from fno.king.state import king_manifest_path, write_manifest

    path = king_manifest_path(scope, state_root=tmp_path / ".fno")
    write_manifest(
        path,
        scope=scope,
        harness_session_id=session,
        shape=shape,
        owner_cwd=str(tmp_path),
    )
    return path


def _isolate(monkeypatch, tmp_path: Path) -> None:
    """Point every king-manifest root this module resolves at the tmp dir.

    ``reign_state`` derives its default root from the caller's git repo (the
    same per-project root the stop hooks pass as --state-root), and a pytest
    run's repo is this worktree, not the fixture's tmp_path.
    """
    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.setattr(
        "fno.king.state._owner_state_root", lambda _cwd=None: tmp_path / ".fno"
    )


def test_ac1_manifest_written_by_init_carries_shape_pass(tmp_path: Path) -> None:
    from fno.king.state import parse_manifest

    path = _write_manifest(tmp_path, "alpha", "aaaa1111-0000-4000-8000-000000000001")
    fields = parse_manifest(path)
    assert fields["shape"] == "pass"
    # A court-shaped write records court verbatim, not normalized away.
    court = _write_manifest(
        tmp_path, "beta", "aaaa2222-0000-4000-8000-000000000002", shape="court"
    )
    assert parse_manifest(court)["shape"] == "court"


def test_ac2_live_crowned_session_answers_scope_and_live(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.registry import write_registry
    from fno.king.state import reign_state

    _isolate(monkeypatch, tmp_path)
    sid = "aaaa1111-0000-4000-8000-000000000001"
    write_registry(
        [_entry("king", status="busy", crown_scope="alpha", harness_session_id=sid)]
    )
    _write_manifest(tmp_path, "alpha", sid)

    class _Ident:
        session_id = sid
        harness = "claude"

    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity", lambda: _Ident()
    )
    state = reign_state()
    assert state.crowned is True
    assert state.scope == "alpha"
    assert state.live is True
    assert state.shape == "pass"
    assert state.split is False


def test_ac3_manifest_and_registry_sessions_differ_is_split(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.registry import write_registry
    from fno.king.state import reign_state

    _isolate(monkeypatch, tmp_path)
    manifest_sid = "aaaa1111-0000-4000-8000-000000000001"
    registry_sid = "bbbb2222-0000-4000-8000-000000000002"
    write_registry(
        [_entry("heir", status="idle", crown_scope="alpha", harness_session_id=registry_sid)]
    )
    _write_manifest(tmp_path, "alpha", manifest_sid)

    state = reign_state(scope="alpha")
    assert state.split is True
    assert state.manifest_session == manifest_sid
    assert state.registry_session == registry_sid


def test_ac4_unreadable_registry_answers_unknown_never_clean_false(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.king.state import reign_state

    _isolate(monkeypatch, tmp_path)

    def _boom():
        raise OSError("disk on fire")

    monkeypatch.setattr("fno.agents.registry.load_registry", _boom)
    state = reign_state(scope="alpha")
    assert state.live is None
    assert state.crowned is None
    assert state.split is None
    assert state.unknown_reason is not None
    assert "registry" in state.unknown_reason


def test_scope_with_no_manifest_keeps_split_none_and_names_it(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.registry import write_registry
    from fno.king.state import reign_state

    _isolate(monkeypatch, tmp_path)
    write_registry([_entry("king", status="busy", crown_scope="alpha")])

    state = reign_state(scope="alpha")
    assert state.crowned is True
    assert state.split is None
    assert state.shape is None
    assert state.unknown_reason is not None
    assert "no manifest" in state.unknown_reason


def test_terminal_crown_row_is_not_a_live_reign(tmp_path: Path, monkeypatch) -> None:
    from fno.agents.registry import write_registry
    from fno.king.state import reign_state

    _isolate(monkeypatch, tmp_path)
    write_registry([_entry("king", status="exited", crown_scope="alpha")])

    state = reign_state(scope="alpha")
    assert state.crowned is False
    assert state.live is False
    assert state.split is None


def test_set_manifest_shape_rewrites_and_is_idempotent(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.king.state import parse_manifest, set_manifest_shape

    use_tmpdir(monkeypatch, tmp_path)
    sid = "aaaa1111-0000-4000-8000-000000000001"
    path = _write_manifest(tmp_path, "alpha", sid)

    assert set_manifest_shape("alpha", "court", state_root=tmp_path / ".fno") == "court"
    assert parse_manifest(path)["shape"] == "court"
    # Second call with the same value is a no-op rewrite, not a refusal.
    assert set_manifest_shape("alpha", "court", state_root=tmp_path / ".fno") == "court"
    assert parse_manifest(path)["shape"] == "court"
    # And back, on the caller's own manifest.
    assert set_manifest_shape(
        "alpha", "pass", state_root=tmp_path / ".fno", expect_session_id=sid
    ) == "pass"


def test_set_manifest_shape_refuses_another_kings_manifest(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.king.state import set_manifest_shape

    use_tmpdir(monkeypatch, tmp_path)
    _write_manifest(tmp_path, "alpha", "aaaa1111-0000-4000-8000-000000000001")

    with pytest.raises(ValueError, match="names session"):
        set_manifest_shape(
            "alpha",
            "court",
            state_root=tmp_path / ".fno",
            expect_session_id="bbbb2222-0000-4000-8000-000000000002",
        )


def test_set_manifest_shape_refuses_without_manifest(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.king.state import set_manifest_shape

    use_tmpdir(monkeypatch, tmp_path)
    with pytest.raises(ValueError, match="no manifest"):
        set_manifest_shape("alpha", "court", state_root=tmp_path / ".fno")
    with pytest.raises(ValueError, match="pass or court"):
        set_manifest_shape("alpha", "siege", state_root=tmp_path / ".fno")
