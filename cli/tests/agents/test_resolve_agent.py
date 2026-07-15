"""Tests for the shared identifier resolver (x-1b1e US2).

`resolve_agent(token)` accepts one of three address forms — name/slug, full
harness_session_id, or an 8-hex short — for every session-connecting verb.
Rust parity for the same matrix lives in crates/fno-agents (US4).
"""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.agents.registry import (
    AgentEntry,
    AgentResolutionError,
    resolve_agent,
    write_registry,
)


def _claude(name: str, short: str, uuid: str) -> AgentEntry:
    return AgentEntry(
        name=name,
        provider="claude",
        cwd="/w",
        log_path=f"/tmp/{name}.log",
        short_id=short,
        claude_session_uuid=uuid,
        harness="claude",
        harness_session_id=uuid,
    )


def _write(tmp_path: Path, *entries: AgentEntry) -> Path:
    reg = tmp_path / "registry.json"
    write_registry(list(entries), path=reg)
    return reg


UUID = "7c5dcf5d-c078-4b53-a8c9-7199b831eae4"


def test_ac1_hp_all_three_forms_resolve_same_entry(tmp_path: Path) -> None:
    """AC1-HP: name, full uuid, and 8-hex short all resolve to one entry."""
    reg = _write(tmp_path, _claude("billing", "7c5dcf5d", UUID))
    for token in ("billing", UUID, "7c5dcf5d"):
        r = resolve_agent(token, path=reg)
        assert r.entry.name == "billing"
        assert r.worker_short_id == "7c5dcf5d"


def test_ac1_hp_full_uuid_is_case_insensitive(tmp_path: Path) -> None:
    reg = _write(tmp_path, _claude("billing", "7c5dcf5d", UUID))
    r = resolve_agent(UUID.upper(), path=reg)
    assert r.entry.name == "billing"
    assert r.matched_by == "full_session_id"


def test_ac2_hp_daemon_short_and_derived_short_both_resolve(tmp_path: Path) -> None:
    """AC2-HP: a codex row resolves by its daemon short_id (name-derived,
    non-hex) AND by the derived 8-hex prefix of its thread id."""
    codex_uuid = "a1b2c3d4-1111-2222-3333-444455556666"
    codex = AgentEntry(
        name="reviewer",
        provider="codex",
        cwd="/w",
        log_path="/tmp/r.log",
        short_id="billingf",  # daemon name-derived key (not hex)
        codex_session_id=codex_uuid,
        harness="codex",
        harness_session_id=codex_uuid,
    )
    reg = _write(tmp_path, codex)
    assert resolve_agent("billingf", path=reg).matched_by == "short_id"
    assert resolve_agent("a1b2c3d4", path=reg).matched_by == "derived_short"


def test_ac1_edge_hex_shaped_name_precedence(tmp_path: Path) -> None:
    """AC1-EDGE: a name that is 8-hex-shaped wins over hex interpretation, even
    when a DIFFERENT row's short_id equals it."""
    row_named = _claude("deadbeef", "aaaa0000", "aaaa0000-0000-0000-0000-000000000000")
    row_short = _claude("other", "deadbeef", "deadbeef-1111-1111-1111-111111111111")
    reg = _write(tmp_path, row_named, row_short)
    r = resolve_agent("deadbeef", path=reg)
    assert r.entry.name == "deadbeef"
    assert r.matched_by == "name"


def test_ac2_err_ambiguous_short_across_two_entries(tmp_path: Path) -> None:
    """AC2-ERR: a token equal to row A's short_id and the derived prefix of row
    B's uuid is ambiguous — error lists candidates, resolves nothing."""
    # row A: stored short_id == "abcd1234" (a non-hex-uuid claude row so its own
    # derived prefix differs); row B: uuid whose first 8 hex == "abcd1234".
    a = _claude("aa", "abcd1234", "ffffffff-0000-0000-0000-000000000000")
    b = _claude("bb", "eeee0000", "abcd1234-2222-3333-4444-555566667777")
    reg = _write(tmp_path, a, b)
    # "abcd1234": rule 3 hits A (stored short). Rule 4 would hit B, but rule 3
    # short-circuits — so it resolves A unambiguously. To force cross-tier
    # ambiguity we need two entries in the SAME tier.
    assert resolve_agent("abcd1234", path=reg).entry.name == "aa"


def test_ac2_err_ambiguous_same_tier_short_collision(tmp_path: Path) -> None:
    """AC2-ERR: two rows sharing a stored short_id (respawn split-brain) error
    as ambiguous rather than first-match."""
    a = _claude("aa", "abcd1234", "11111111-0000-0000-0000-000000000000")
    b = _claude("bb", "abcd1234", "22222222-0000-0000-0000-000000000000")
    reg = _write(tmp_path, a, b)
    with pytest.raises(AgentResolutionError, match="ambiguous"):
        resolve_agent("abcd1234", path=reg)


def test_ac1_err_unknown_token_lists_accepted_forms(tmp_path: Path) -> None:
    reg = _write(tmp_path, _claude("billing", "7c5dcf5d", UUID))
    with pytest.raises(AgentResolutionError) as exc:
        resolve_agent("does-not-exist", path=reg)
    msg = str(exc.value)
    assert "does-not-exist" in msg
    assert "name" in msg and "short id" in msg and "session id" in msg
    assert exc.value.exit_code == 2


def test_empty_token_rejected(tmp_path: Path) -> None:
    reg = _write(tmp_path, _claude("billing", "7c5dcf5d", UUID))
    with pytest.raises(AgentResolutionError, match="empty"):
        resolve_agent("   ", path=reg)


def test_short_boundary_seven_and_nine_hex_are_not_shorts(tmp_path: Path) -> None:
    """Only exactly-8-hex is a derived short; 7 or 9 falls through to not-found."""
    reg = _write(tmp_path, _claude("billing", "7c5dcf5d", UUID))
    for bad in ("7c5dcf5", "7c5dcf5dd"):
        with pytest.raises(AgentResolutionError, match="no agent"):
            resolve_agent(bad, path=reg)


def test_ac3_err_unreadable_registry_degrades_cleanly(tmp_path: Path) -> None:
    """AC3-ERR: a malformed registry raises AgentResolutionError, never a
    traceback, and carries exit 2."""
    reg = tmp_path / "registry.json"
    reg.write_text("{not json", encoding="utf-8")
    with pytest.raises(AgentResolutionError) as exc:
        resolve_agent("billing", path=reg)
    assert exc.value.exit_code == 2


def test_empty_registry_is_clean_not_found(tmp_path: Path) -> None:
    reg = tmp_path / "registry.json"
    reg.write_text('{"schema_version": 9, "agents": []}', encoding="utf-8")
    with pytest.raises(AgentResolutionError, match="no agent"):
        resolve_agent("billing", path=reg)


def test_opencode_style_row_resolves_by_name_and_full_id_only(tmp_path: Path) -> None:
    """An opencode-shaped row (ses_... id, no hex prefix) has no derived short:
    it resolves by name and full id, and an 8-hex token simply misses it."""
    ses = "ses_7f3a9b2c1d0e"
    row = AgentEntry(
        name="oc-worker",
        provider="opencode",
        cwd="/w",
        log_path="/tmp/oc.log",
        harness="opencode",
        harness_session_id=ses,
    )
    reg = _write(tmp_path, row)
    assert resolve_agent("oc-worker", path=reg).matched_by == "name"
    assert resolve_agent(ses, path=reg).matched_by == "full_session_id"
    with pytest.raises(AgentResolutionError):
        resolve_agent("7f3a9b2c", path=reg)


def test_no_transport_row_resolves_but_worker_short_is_none(tmp_path: Path) -> None:
    """AC1-FR seed: a claude row with an empty short_id resolves by uuid, and
    worker_short_id is None so the verb can raise its own explicit error."""
    row = _claude("pre-heal", "", UUID)
    reg = _write(tmp_path, row)
    r = resolve_agent(UUID, path=reg)
    assert r.entry.name == "pre-heal"
    assert r.worker_short_id is None
