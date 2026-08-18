"""Tests for fno.agents.read — list_agents() core logic.

Covers AC1-HP (populated table), AC1-EDGE (empty), AC1-UI (--json forces JSON),
AC3-HP (cross-provider shape), AC3-EDGE (filter intersection).

The claude_agents_json() shellout is mocked via monkeypatch of the
``fno.agents.harnesses.claude.claude_agents_json`` function symbol.
Live shellout is exercised in providers test + integration smoke.
"""
from __future__ import annotations

import json

import pytest

from fno.agents.read import list_agents
from fno.agents.registry import AgentEntry, write_registry
from fno.paths_testing import use_tmpdir


def _claude(**kw) -> AgentEntry:
    base = dict(
        name="worker-frontend",
        harness="claude",
        cwd="/Users/foo/code/proj",
        log_path="/Users/foo/.fno/agents/worker-frontend/output.jsonl",
        short_id="abc12345",
        created_at="2026-05-20T17:00:00Z",
        status="live",
        last_message_at="2026-05-20T17:30:12Z",
    )
    base.update(kw)
    return AgentEntry(**base)


def _codex(**kw) -> AgentEntry:
    base = dict(
        name="worker-migration",
        harness="codex",
        cwd="/Users/foo/code/proj",
        log_path="/Users/foo/.fno/agents/worker-migration/output.jsonl",
        harness_session_id="codex-sess-xyz",
        created_at="2026-05-20T17:15:00Z",
        status="live",
        last_message_at="2026-05-20T17:15:43Z",
    )
    base.update(kw)
    return AgentEntry(**base)


@pytest.fixture
def _patch_claude_agents_json(monkeypatch):
    """Return a function that installs a fake claude_agents_json result."""
    def _install(result, warnings=None):
        from fno.agents.harnesses import claude as claude_mod

        def _fake(timeout=3.0):  # noqa: ARG001
            return result, list(warnings or [])

        monkeypatch.setattr(claude_mod, "claude_agents_json", _fake)
    return _install


def test_list_agents_populated_table(tmp_path, monkeypatch, _patch_claude_agents_json):
    """AC1-HP — three entries render with their respective LIVE columns."""
    use_tmpdir(monkeypatch, tmp_path)
    write_registry(
        [
            _claude(name="alpha"),
            _codex(name="bravo"),
            _claude(name="charlie", status="orphaned"),
        ]
    )
    _patch_claude_agents_json(
        {"abc12345": {"live_status": "Working"}}
    )

    result = list_agents(json_out=False, tty=True)

    assert result.exit_code == 0
    assert "alpha" in result.output
    assert "bravo" in result.output
    assert "charlie" in result.output
    assert "Working" in result.output  # claude live entry's LIVE column
    assert result.warnings == []


def test_list_agents_empty_registry_json_shape(tmp_path, monkeypatch, _patch_claude_agents_json):
    """AC1-EDGE — empty registry returns valid empty shape."""
    use_tmpdir(monkeypatch, tmp_path)
    _patch_claude_agents_json({})  # noqa: F841

    result = list_agents(json_out=True, tty=True)

    parsed = json.loads(result.output)
    assert parsed == {
        "agents": [],
        "count": 0,
        "discovered_sessions": [],
        "discovered_count": 0,
        "filters_applied": {"cwd": None, "provider": None, "status": None, "progress": None},
        "schema_version": 2,
    }
    assert result.exit_code == 0


def test_list_agents_json_flag_forces_json_in_tty(
    tmp_path, monkeypatch, _patch_claude_agents_json
):
    """AC1-UI — --json forces JSON regardless of TTY."""
    use_tmpdir(monkeypatch, tmp_path)
    write_registry([_claude(name="alpha")])
    _patch_claude_agents_json({"abc12345": {"live_status": "Idle"}})

    result = list_agents(json_out=True, tty=True)

    parsed = json.loads(result.output)
    assert parsed["count"] == 1
    assert parsed["agents"][0]["name"] == "alpha"
    assert parsed["agents"][0]["live_status"] == "Idle"


def test_list_agents_non_tty_default_emits_json(
    tmp_path, monkeypatch, _patch_claude_agents_json
):
    """Locked Decision 4 — non-TTY stdout defaults to JSON."""
    use_tmpdir(monkeypatch, tmp_path)
    write_registry([_claude(name="alpha")])
    _patch_claude_agents_json({"abc12345": {"live_status": "Idle"}})

    result = list_agents(json_out=False, tty=False)

    # JSON renderer output begins with `{`.
    assert result.output.lstrip().startswith("{")
    json.loads(result.output)  # parseable


def test_list_agents_cross_provider_shape_stable(
    tmp_path, monkeypatch, _patch_claude_agents_json
):
    """AC3-HP — JSON shape stable across providers."""
    use_tmpdir(monkeypatch, tmp_path)
    write_registry([_claude(), _codex()])
    _patch_claude_agents_json({"abc12345": {"live_status": "Working"}})

    result = list_agents(json_out=True, tty=True)
    parsed = json.loads(result.output)

    keysets = [frozenset(a.keys()) for a in parsed["agents"]]
    assert len(set(keysets)) == 1  # identical key set


def test_list_agents_filter_by_provider(
    tmp_path, monkeypatch, _patch_claude_agents_json
):
    use_tmpdir(monkeypatch, tmp_path)
    write_registry([_claude(name="a"), _codex(name="b")])
    _patch_claude_agents_json({"abc12345": {"live_status": "Working"}})

    result = list_agents(provider="codex", json_out=True, tty=True)
    parsed = json.loads(result.output)

    assert parsed["count"] == 1
    assert parsed["agents"][0]["name"] == "b"
    assert parsed["filters_applied"]["provider"] == "codex"


def test_list_agents_filter_by_status(
    tmp_path, monkeypatch, _patch_claude_agents_json
):
    use_tmpdir(monkeypatch, tmp_path)
    write_registry(
        [
            _claude(name="alive", status="live"),
            _claude(
                name="dead",
                status="orphaned",
                short_id="def67890",
            ),
        ]
    )
    _patch_claude_agents_json({"abc12345": {"live_status": "Working"}})
    from fno.agents import session_truth

    monkeypatch.setattr(
        session_truth,
        "resolve_session_truth",
        lambda handle, **_kwargs: {
            "state": "done" if handle == "dead" else "working"
        },
    )

    # `done` means the worker declared its MISSION complete, which says nothing
    # about whether it is still up (x-16d8). It is UNKNOWN, not orphaned; only
    # an affirmative falsifier condemns a row.
    result = list_agents(status="unknown", json_out=True, tty=True)
    parsed = json.loads(result.output)

    assert parsed["count"] == 1
    assert parsed["agents"][0]["name"] == "dead"

    assert json.loads(
        list_agents(status="orphaned", json_out=True, tty=True).output
    )["count"] == 0


def test_list_agents_filter_by_progress_is_independent_of_status(
    tmp_path, monkeypatch, _patch_claude_agents_json
):
    """AC15-HP: the two axes filter independently.

    The exact motivating specimen: both rows are ``working`` and reachable
    (``status == "live"`` on both), and one is answering as a foreign model.
    ``--progress refused`` narrows to that row alone, while ``--status live``
    still returns both -- a refused worker is fully reachable.
    """
    use_tmpdir(monkeypatch, tmp_path)
    write_registry(
        [
            _claude(name="healthy", status="live"),
            _claude(name="refused", status="live", short_id="def67890"),
        ]
    )
    _patch_claude_agents_json(
        {"abc12345": {"live_status": "Working"}, "def67890": {"live_status": "Working"}}
    )
    from fno.agents import session_truth

    def _fake_truth(handle, **_kwargs):
        model = (
            {"kind": "observed", "model": "glm-5.2[1m]"}
            if handle == "refused"
            else {"kind": "observed", "model": "claude-opus-4-1"}
        )
        return {"state": "working", "observed_model": model}

    monkeypatch.setattr(session_truth, "resolve_session_truth", _fake_truth)

    refused_only = json.loads(
        list_agents(progress="refused", json_out=True, tty=True).output
    )
    assert refused_only["count"] == 1
    assert refused_only["agents"][0]["name"] == "refused"
    assert refused_only["filters_applied"]["progress"] == "refused"

    still_live = json.loads(
        list_agents(status="live", json_out=True, tty=True).output
    )
    assert still_live["count"] == 2, "the status axis must not shrink when progress is filtered separately"


def test_list_agents_filter_by_cwd_resolves_relative(
    tmp_path, monkeypatch, _patch_claude_agents_json
):
    """AC3-EDGE — cwd filter resolves relative paths to absolute first."""
    use_tmpdir(monkeypatch, tmp_path)
    target_cwd = (tmp_path / "subdir").resolve()
    target_cwd.mkdir()
    write_registry([_claude(name="a", cwd=str(target_cwd))])
    _patch_claude_agents_json({"abc12345": {"live_status": "Working"}})

    monkeypatch.chdir(tmp_path)
    # Relative filter "./subdir" should resolve to target_cwd absolute.
    result = list_agents(cwd="./subdir", json_out=True, tty=True)
    parsed = json.loads(result.output)

    assert parsed["count"] == 1
    # filters_applied preserves the resolved absolute path.
    assert parsed["filters_applied"]["cwd"] == str(target_cwd)


def test_list_agents_filter_intersection_zero_matches(
    tmp_path, monkeypatch, _patch_claude_agents_json
):
    """AC3-EDGE — filters with no matches return empty agents + filters_applied."""
    use_tmpdir(monkeypatch, tmp_path)
    write_registry([_claude(), _codex()])
    _patch_claude_agents_json({"abc12345": {"live_status": "Working"}})

    result = list_agents(
        cwd="/nonexistent", provider="gemini", json_out=True, tty=True
    )
    parsed = json.loads(result.output)

    assert parsed["count"] == 0
    assert parsed["agents"] == []
    assert parsed["filters_applied"]["cwd"] == "/nonexistent"
    assert parsed["filters_applied"]["provider"] == "gemini"


def test_list_agents_claude_shellout_failure_falls_back(
    tmp_path, monkeypatch, _patch_claude_agents_json
):
    """AC1-FR — claude shellout fail → live_status=null, list still 0."""
    use_tmpdir(monkeypatch, tmp_path)
    write_registry(
        [
            _claude(name="a"),
            _claude(name="b", short_id="def67890"),
        ]
    )
    # Empty result + warning simulates the fallback path.
    _patch_claude_agents_json(
        {}, warnings=["claude agents --json failed: command not found"]
    )

    result = list_agents(json_out=True, tty=True)
    parsed = json.loads(result.output)

    assert parsed["count"] == 2
    assert all(a["live_status"] is None for a in parsed["agents"])
    assert any("claude agents" in w for w in result.warnings)
    assert result.exit_code == 0


def test_list_agents_corrupt_registry_exits_1(tmp_path, monkeypatch, _patch_claude_agents_json):
    """AC1-ERR — corrupt JSON exits 1 with file path + parser error in warnings."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno import paths

    target = paths.agents_registry_path()
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text("{not valid json", encoding="utf-8")
    _patch_claude_agents_json({})

    result = list_agents(json_out=True, tty=True)

    assert result.exit_code == 1
    assert any(str(target) in w for w in result.warnings)
    # The parser error message should mention what went wrong somewhere.
    assert result.output == ""


def test_list_agents_claude_shellout_only_called_once_per_invocation(
    tmp_path, monkeypatch
):
    """Locked Decision 5b — shell out once per call, no cross-call cache."""
    use_tmpdir(monkeypatch, tmp_path)
    write_registry(
        [
            _claude(name="a"),
            _claude(name="b", short_id="def67890"),
        ]
    )

    calls = {"count": 0}
    from fno.agents.harnesses import claude as claude_mod

    def _fake(timeout=3.0):  # noqa: ARG001
        calls["count"] += 1
        return {}, []

    monkeypatch.setattr(claude_mod, "claude_agents_json", _fake)

    list_agents(json_out=True, tty=True)
    list_agents(json_out=True, tty=True)

    # Each invocation produces exactly one shellout (per spec).
    assert calls["count"] == 2


def test_list_agents_does_not_mutate_registry(
    tmp_path, monkeypatch, _patch_claude_agents_json
):
    """Locked Decision 5 — list is pure-read, registry file unchanged."""
    use_tmpdir(monkeypatch, tmp_path)
    write_registry([_claude(name="a")])
    _patch_claude_agents_json({"abc12345": {"live_status": "Working"}})

    from fno import paths

    before = paths.agents_registry_path().read_text(encoding="utf-8")
    list_agents(json_out=True, tty=True)
    after = paths.agents_registry_path().read_text(encoding="utf-8")

    assert before == after


def test_list_agents_family1_truth_overrides_stale_orphaned_render(
    tmp_path, monkeypatch, _patch_claude_agents_json
):
    """A pid-less row with a fresh transcript must not render as dead."""
    use_tmpdir(monkeypatch, tmp_path)
    write_registry(
        [
            _codex(
                name="live-codex",
                status="orphaned",
                pid=None,
                harness_session_id="019f8ff2-1111-2222-3333-444444444444",
            )
        ]
    )
    _patch_claude_agents_json({})
    from fno.agents import session_truth

    monkeypatch.setattr(
        session_truth,
        "resolve_session_truth",
        lambda *_args, **_kwargs: {"state": "working", "last_activity_age_s": 1},
    )

    row = json.loads(list_agents(json_out=True, tty=True).output)["agents"][0]
    assert row["status"] == "live"


def test_list_agents_unknown_truth_never_inherits_registry_death(
    tmp_path, monkeypatch, _patch_claude_agents_json
):
    """Registry metadata cannot fill an inconclusive family-1 verdict."""
    use_tmpdir(monkeypatch, tmp_path)
    write_registry([_codex(name="uncertain", status="orphaned", pid=None)])
    _patch_claude_agents_json({})
    from fno.agents import session_truth

    monkeypatch.setattr(
        session_truth,
        "resolve_session_truth",
        lambda *_args, **_kwargs: {"state": "unknown", "reason": "no-records"},
    )

    row = json.loads(list_agents(json_out=True, tty=True).output)["agents"][0]
    assert row["status"] == "unknown"


# --------------------------------------------------------------------------
# Discovered-lane provider filter
# --------------------------------------------------------------------------


class _Discovered:
    """Minimal DiscoveredSession stand-in (list_agents calls .to_row())."""

    def __init__(self, agent: str, short_id: str, status: str = "live"):
        self.agent = agent
        self.short_id = short_id
        self.cwd = ""
        self.status = status

    def to_row(self) -> dict:
        # `status` is not optional on a real row: the shared reachability verdict
        # is applied inside `DiscoveredSession.to_row`, and the lane filters on
        # what the ROW says rather than re-deriving it. A stub that omitted the
        # key made the whole lane vanish behind the broad except.
        return {
            "agent": self.agent,
            "short_id": self.short_id,
            "handle": self.short_id,
            "status": self.status,
        }


@pytest.fixture
def _patch_discovery(monkeypatch):
    def _install(sessions):
        from fno.agents import discover as discover_mod

        monkeypatch.setattr(
            discover_mod, "discover_live_sessions", lambda **kw: list(sessions)
        )
    return _install


def _discovered_agents(payload: str) -> list[str]:
    return [r["agent"] for r in json.loads(payload)["discovered_sessions"]]


@pytest.mark.parametrize(
    "provider,expected",
    [
        (None, ["claude", "codex", "opencode"]),
        ("claude", ["claude"]),
        ("codex", ["codex"]),
        ("opencode", ["opencode"]),
    ],
)
def test_discovered_rows_honor_provider_filter(
    tmp_path, monkeypatch, _patch_claude_agents_json, _patch_discovery, provider, expected
):
    """A discovered row must be filtered by its OWN harness.

    The lane used to be gated on `provider in (None, "claude")`, so
    `--provider claude` listed every discovered codex/opencode session and
    `--provider codex` listed none of them.
    """
    use_tmpdir(monkeypatch, tmp_path)
    _patch_claude_agents_json([])
    write_registry([], path=tmp_path / "registry.json")
    _patch_discovery(
        [
            _Discovered("claude", "aaaa1111"),
            _Discovered("codex", "bbbb2222"),
            _Discovered("opencode", "cccc3333"),
        ]
    )
    result = list_agents(provider=provider, json_out=True)
    assert _discovered_agents(result.output) == expected


@pytest.mark.parametrize(
    "status,expected",
    [
        (None, ["claude"]),
        ("live", ["claude"]),
        ("orphaned", []),
        ("unknown", []),
    ],
)
def test_discovered_live_rows_honor_status_filter(
    tmp_path,
    monkeypatch,
    _patch_claude_agents_json,
    _patch_discovery,
    status,
    expected,
):
    use_tmpdir(monkeypatch, tmp_path)
    _patch_claude_agents_json([])
    write_registry([], path=tmp_path / "registry.json")
    _patch_discovery([_Discovered("claude", "aaaa1111")])

    result = list_agents(status=status, json_out=True)

    assert _discovered_agents(result.output) == expected


def test_a_falsified_discovered_row_is_surfaced_orphaned_not_dropped(
    tmp_path, monkeypatch, _patch_claude_agents_json, _patch_discovery
):
    """One session, one answer, whichever runtime rendered the lane.

    `fno agents discovered-json` (what the Rust `list` shells out to) emits a
    pid-falsified session with `status: orphaned`. This lane must agree instead
    of dropping the row, or `fno agents list --json` and `list_agents(...)`
    disagree about whether the session exists at all.
    """
    use_tmpdir(monkeypatch, tmp_path)
    _patch_claude_agents_json([])
    write_registry([], path=tmp_path / "registry.json")
    _patch_discovery([_Discovered("claude", "aaaa1111", status="orphaned")])

    assert _discovered_agents(list_agents(json_out=True).output) == ["claude"]
    assert _discovered_agents(
        list_agents(status="orphaned", json_out=True).output
    ) == ["claude"]
    assert _discovered_agents(list_agents(status="live", json_out=True).output) == []
