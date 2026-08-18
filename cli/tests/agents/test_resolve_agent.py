"""Tests for the shared identifier resolver (x-1b1e US2).

`resolve_agent(token)` accepts one of three address forms — name/slug, full
harness_session_id, or an 8-hex short — for every session-connecting verb.
Rust parity for the same matrix lives in crates/fno-agents (US4).
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.agents.registry import (
    AgentEntry,
    AgentResolutionError,
    register_existing_session,
    resolve_agent,
    resolve_agent_in,
    update_registry,
    write_registry,
)


def _claude(name: str, short: str, uuid: str) -> AgentEntry:
    return AgentEntry(
        name=name,
        cwd="/w",
        log_path=f"/tmp/{name}.log",
        short_id=short,
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


def test_ac2_hp_daemon_short_and_canonical_handle_both_resolve(tmp_path: Path) -> None:
    """AC2-HP: a codex row resolves by its daemon short_id (name-derived,
    non-hex) AND by the canonical first-eight of its thread id."""
    codex_uuid = "a1b2c3d4-1111-2222-3333-444455556666"
    codex = AgentEntry(
        name="reviewer",
        cwd="/w",
        log_path="/tmp/r.log",
        short_id="billingf",  # daemon name-derived key (not hex)
        harness="codex",
        harness_session_id=codex_uuid,
    )
    reg = _write(tmp_path, codex)
    assert resolve_agent("billingf", path=reg).matched_by == "short_id"
    assert resolve_agent("a1b2c3d4", path=reg).matched_by == "canonical_handle"


def test_ac2_hp_opencode_canonical_handle_preserves_case() -> None:
    ses = "ses_7F3a9b2cAbCd1234"
    row = AgentEntry(
        name="oc", harness="opencode", harness_session_id=ses, cwd="/w", log_path="/l"
    )
    assert resolve_agent_in([row], "ses_7F3a").matched_by == "canonical_handle"
    with pytest.raises(AgentResolutionError):
        resolve_agent_in([row], "ses_7f3a")


def test_update_registry_allows_first_eight_overlap_between_sessions(
    tmp_path: Path,
) -> None:
    """Two rows whose sids share first-eight but name different sessions both
    land: the overlap (any harness, claude included) is read-side ambiguity,
    not a write collision. Codex hits this on every tiled same-window spawn."""
    reg = _write(
        tmp_path,
        _claude("first", "transport1", "aaaaaaaa-0000-0000-0000-111111111111"),
    )
    second = _claude(
        "second", "transport2", "aaaaaaaa-0000-0000-0000-222222222222"
    )

    persisted = update_registry(lambda rows: [*rows, second], path=reg)

    assert [entry.name for entry in persisted] == ["first", "second"]
    with pytest.raises(AgentResolutionError, match="ambiguous"):
        resolve_agent("aaaaaaaa", path=reg)
    by_full = resolve_agent(
        "aaaaaaaa-0000-0000-0000-222222222222", path=reg
    )
    assert by_full.entry.name == "second"


def test_update_registry_refuses_name_shadowing_existing_handle(tmp_path: Path) -> None:
    reg = _write(
        tmp_path,
        _claude("first", "transport1", "aaaaaaaa-0000-0000-0000-1111deadbeef"),
    )
    shadow = _claude(
        "deadbeef", "transport2", "bbbbbbbb-0000-0000-0000-222233334444"
    )

    with pytest.raises(AgentResolutionError, match="collides with row 'first'"):
        update_registry(lambda rows: [*rows, shadow], path=reg)


def test_update_registry_allows_legacy_suffix_collision(tmp_path: Path) -> None:
    """Retired last-eight handles remain ambiguity-compatible, not a spawn wall."""
    reg = _write(
        tmp_path,
        _claude("first", "transport1", "019fb417-0000-0000-0000-111122223333"),
    )
    second = _claude(
        "second", "transport2", "019fb418-0000-0000-0000-000012223333"
    )

    persisted = update_registry(lambda rows: [*rows, second], path=reg)

    assert [entry.name for entry in persisted] == ["first", "second"]


def _codex(name: str, uuid: str, short: str | None = None) -> AgentEntry:
    return AgentEntry(
        name=name,
        cwd="/w",
        log_path=f"/tmp/{name}.log",
        short_id=short,
        harness="codex",
        harness_session_id=uuid,
    )


# Codex session ids are UUIDv7: the first eight hex chars are the top 32 bits
# of a 48-bit millisecond timestamp, so sessions started in one ~65s window
# share first-eight exactly. A tiled spawn batch therefore mints rows whose
# canonical handles overlap while the full ids stay distinct.
_WINDOW_A = "01a0152f-45fd-78f0-b109-78f8dffdeeca"
_WINDOW_B = "01a0152f-9a2b-74c3-8b0f-11aa22bb33cc"


def test_update_registry_allows_same_window_codex_pair_null_short(
    tmp_path: Path,
) -> None:
    """The live spawn shape: two codex rows, no short_id, one time window.

    A first-eight overlap between two DIFFERENT sessions is the documented
    same-window shape; the write must land and resolution must fail closed
    asking for the full id, not the spawn die at the registry write.
    """
    reg = _write(tmp_path, _codex("gql-codex", _WINDOW_A))

    persisted = update_registry(
        lambda rows: [*rows, _codex("preflight-codex", _WINDOW_B)], path=reg
    )

    assert [entry.name for entry in persisted] == ["gql-codex", "preflight-codex"]


def test_update_registry_allows_same_window_pair_with_populated_shorts(
    tmp_path: Path,
) -> None:
    """Populating short_id does not change the rule: the collision compared
    the minted handle against the stored session id, never the stored short."""
    reg = _write(tmp_path, _claude("first", "transport1", "aaaaaaaa-0000-0000-0000-1"))

    persisted = update_registry(
        lambda rows: [*rows, _claude("second", "transport2", "aaaaaaaa-0000-0000-0000-2")],
        path=reg,
    )

    assert [entry.name for entry in persisted] == ["first", "second"]


def test_resolve_shared_first_eight_fails_closed_ambiguous(tmp_path: Path) -> None:
    """Safety moved to the read side: the shared short resolves nothing."""
    reg = _write(
        tmp_path,
        _codex("gql-codex", _WINDOW_A),
        _codex("preflight-codex", _WINDOW_B),
    )
    with pytest.raises(AgentResolutionError, match="ambiguous"):
        resolve_agent("01a0152f", path=reg)
    assert resolve_agent(_WINDOW_B, path=reg).entry.name == "preflight-codex"


def test_update_registry_refuses_second_row_claiming_same_full_session_id(
    tmp_path: Path,
) -> None:
    reg = _write(tmp_path, _codex("gql-codex", _WINDOW_A))
    duplicate = _codex("clone-codex", _WINDOW_A)

    with pytest.raises(AgentResolutionError, match="collides with row 'gql-codex'"):
        update_registry(lambda rows: [*rows, duplicate], path=reg)


def test_update_registry_null_id_row_never_collides(tmp_path: Path) -> None:
    """A row with no short and no session id has no address to match: a null
    stored id must never read as equal to a new row's minted identity."""
    bare = AgentEntry(
        name="legacy-row", harness="claude", cwd="/w", log_path="/tmp/legacy.log"
    )
    reg = _write(tmp_path, bare)

    persisted = update_registry(
        lambda rows: [*rows, _codex("fresh-codex", _WINDOW_A)], path=reg
    )

    assert [entry.name for entry in persisted] == ["legacy-row", "fresh-codex"]


def test_register_existing_session_allows_same_window_generated_handles(
    tmp_path: Path,
) -> None:
    """Hand-started sessions in one codex time window both register: the
    generated name suffixes on collision, the shared short fails closed at
    read, and neither registration is refused."""
    reg = tmp_path / "registry.json"

    first = register_existing_session(
        provider="codex", session_id=_WINDOW_A, cwd="/w", registry_path=reg
    )
    second = register_existing_session(
        provider="codex", session_id=_WINDOW_B, cwd="/w", registry_path=reg
    )

    assert first.name == "01a0152f"
    assert second.name == "01a0152f-2"
    with pytest.raises(AgentResolutionError, match="ambiguous"):
        resolve_agent("01a0152f", path=reg)


def test_ac4_err_canonical_handle_and_legacy_prefix_are_ambiguous() -> None:
    canonical = _claude("canonical", "transport1", "ffffffff-0000-0000-0000-abcd1234")
    legacy = _claude("legacy", "transport2", "abcd1234-0000-0000-0000-ffffffff")
    with pytest.raises(AgentResolutionError, match="ambiguous"):
        resolve_agent_in([legacy, canonical], "abcd1234")


def test_ac4_err_legacy_prefix_collision_is_ambiguous() -> None:
    rows = [
        _claude("one", "transport1", "019fb417-0000-0000-0000-11111111"),
        _claude("two", "transport2", "019fb417-0000-0000-0000-22222222"),
    ]
    with pytest.raises(AgentResolutionError, match="ambiguous"):
        resolve_agent_in(rows, "019fb417")


def test_ac1_edge_hex_shaped_name_and_short_id_are_ambiguous(tmp_path: Path) -> None:
    """AC1-EDGE: a name cannot silently displace another row's short id."""
    row_named = _claude("deadbeef", "aaaa0000", "aaaa0000-0000-0000-0000-000000000000")
    row_short = _claude("other", "deadbeef", "deadbeef-1111-1111-1111-111111111111")
    reg = _write(tmp_path, row_named, row_short)
    with pytest.raises(AgentResolutionError, match="ambiguous"):
        resolve_agent("deadbeef", path=reg)


def test_ac2_err_ambiguous_short_across_two_entries(tmp_path: Path) -> None:
    """AC2-ERR: a token equal to row A's short_id and the derived prefix of row
    B's uuid is ambiguous — error lists candidates, resolves nothing."""
    # row A: stored short_id == "abcd1234" (a non-hex-uuid claude row so its own
    # derived prefix differs); row B: uuid whose first 8 hex == "abcd1234".
    a = _claude("aa", "abcd1234", "ffffffff-0000-0000-0000-000000000000")
    b = _claude("bb", "eeee0000", "abcd1234-2222-3333-4444-555566667777")
    reg = _write(tmp_path, a, b)
    with pytest.raises(AgentResolutionError, match="ambiguous"):
        resolve_agent("abcd1234", path=reg)


def test_same_row_matching_multiple_address_categories_is_not_ambiguous() -> None:
    row = _claude(
        "deadbeef", "deadbeef", "deadbeef-0000-0000-0000-0000deadbeef"
    )
    resolved = resolve_agent_in([row], "deadbeef")
    assert resolved.entry.name == "deadbeef"
    assert resolved.matched_by == "name"


def test_duplicate_name_rows_with_distinct_sessions_are_ambiguous() -> None:
    first = _claude(
        "same", "transport1", "aaaaaaaa-1111-7222-8333-4444deadbeef"
    )
    second = _claude(
        "same", "transport2", "bbbbbbbb-1111-7222-8333-4444cafefeed"
    )

    with pytest.raises(AgentResolutionError, match="ambiguous across 2 agents"):
        resolve_agent_in([first, second], "same")


def test_exact_full_session_id_wins_over_short_address_categories() -> None:
    full = _claude("full", "transport1", "deadbeef")
    named = _claude(
        "deadbeef", "transport2", "aaaaaaaa-0000-0000-0000-000000000000"
    )
    resolved = resolve_agent_in([named, full], "deadbeef")
    assert resolved.entry.name == "full"
    assert resolved.matched_by == "full_session_id"


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


def test_opencode_style_row_resolves_by_name_full_id_and_canonical_handle(tmp_path: Path) -> None:
    """An opencode row gains a generated handle without changing its full id."""
    ses = "ses_7f3a9b2cAbCd1234"
    row = AgentEntry(
        name="oc-worker",
        cwd="/w",
        log_path="/tmp/oc.log",
        harness="opencode",
        harness_session_id=ses,
    )
    reg = _write(tmp_path, row)
    assert resolve_agent("oc-worker", path=reg).matched_by == "name"
    assert resolve_agent(ses, path=reg).matched_by == "full_session_id"
    assert resolve_agent("ses_7f3a", path=reg).matched_by == "canonical_handle"


def test_registry_name_and_persisted_alias_share_one_namespace(
    tmp_path: Path, monkeypatch
) -> None:
    """A registry name cannot hide another session's persisted alias."""
    from fno.agents import discover

    registered = _claude("friendly", "transport", UUID)
    alias_sid = "aaaaaaaa-1111-7222-8333-444455556666"
    alias_map = tmp_path / "session-names.json"
    alias_map.write_text(
        json.dumps({alias_sid: "friendly"}),
        encoding="utf-8",
    )
    monkeypatch.setattr(discover, "default_name_map_path", lambda: alias_map)
    reg = _write(tmp_path, registered)

    with pytest.raises(AgentResolutionError, match=alias_sid):
        resolve_agent("friendly", path=reg)


def test_registry_and_alias_same_uuid_different_case_are_one_session(
    tmp_path: Path, monkeypatch
) -> None:
    """UUID-family identity is case-insensitive across registry and alias stores."""
    from fno.agents import discover

    registered = _claude("friendly", "transport", UUID.lower())
    alias_map = tmp_path / "session-names.json"
    alias_map.write_text(
        json.dumps({UUID.upper(): "friendly"}),
        encoding="utf-8",
    )
    monkeypatch.setattr(discover, "default_name_map_path", lambda: alias_map)
    reg = _write(tmp_path, registered)

    assert resolve_agent("friendly", path=reg).entry.name == "friendly"


def test_registry_and_store_same_uuid_different_case_are_one_session(
    tmp_path: Path, monkeypatch
) -> None:
    """A store echo of one UUID cannot become a false ambiguity by case alone."""
    from fno.agents import discover
    from fno.agents import store_fallback
    from fno.agents.store_fallback import StoreHit

    session_id = "aaaaaaaa-1111-7222-8333-444455556666"
    registered = AgentEntry(
        name="worker",
        cwd="/w",
        log_path="/tmp/worker.log",
        harness="codex",
        harness_session_id=session_id.upper(),
    )
    monkeypatch.setattr(
        discover, "default_name_map_path", lambda: tmp_path / "missing-aliases.json"
    )
    monkeypatch.setattr(
        store_fallback,
        "complete_store_hits",
        lambda _token: [StoreHit("codex", session_id.lower(), "/w")],
    )
    reg = _write(tmp_path, registered)

    assert resolve_agent("55556666", path=reg).entry.name == "worker"


def test_no_transport_row_resolves_but_worker_short_is_none(tmp_path: Path) -> None:
    """AC1-FR seed: a claude row with an empty short_id resolves by uuid, and
    worker_short_id is None so the verb can raise its own explicit error."""
    row = _claude("pre-heal", "", UUID)
    reg = _write(tmp_path, row)
    r = resolve_agent(UUID, path=reg)
    assert r.entry.name == "pre-heal"
    assert r.worker_short_id is None
