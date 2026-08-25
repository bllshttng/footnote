"""Tests for shared ambient harness session identity resolution."""

from __future__ import annotations

import os
from pathlib import Path

import pytest

from fno.harness_identity import (
    AMBIENT_IDENTITY_ENV,
    HARNESS_SESSION_MARKERS,
    HarnessIdentity,
    LEGACY_HANDLE_RE,
    OwnedHarnessIdentity,
    canonical_handle,
    claude_transport_short_id,
    current_session_id,
    current_session_ids,
    legacy_suffix_handle,
    present_harness_markers,
    resolve_harness_identity,
    resolve_owned_identity,
    session_identity_key,
)


@pytest.mark.parametrize(
    ("marker", "session_id", "harness"),
    [
        ("CODEX_THREAD_ID", "thread-1", "codex"),
        ("CLAUDE_CODE_SESSION_ID", "claude-1", "claude"),
        ("CODEX_SESSION_ID", "codex-1", "codex"),
        ("GEMINI_SESSION_ID", "gemini-1", "gemini"),
        ("OPENCODE_SESSION_ID", "ses_OpenCode1", "opencode"),
    ],
)
def test_resolves_each_supported_marker(marker, session_id, harness):
    assert resolve_harness_identity({marker: f"  {session_id}  "}) == HarnessIdentity(
        session_id=session_id,
        harness=harness,
    )


def test_precedence_favors_codex_thread_id():
    """Within ONE family, higher-precedence markers still win (thread id over
    the legacy codex session id). Cross-family never gets here - see the
    refusal test below."""
    env = {
        "CODEX_THREAD_ID": "thread",
        "CODEX_SESSION_ID": "codex-session",
    }
    assert resolve_harness_identity(env) == HarnessIdentity("thread", "codex")


def test_cross_family_markers_refuse_instead_of_precedence():
    """x-b57a: markers from two harness families refuse, they do not pick.

    The precedence winner here is a foreign inherited marker laundered into
    this session's identity - the leak that made whoami report harness codex
    for a claude session carrying CODEX_THREAD_ID. The refusal makes this
    resolver agree with infer_invoking_harness on every environment."""
    env = {
        "CODEX_THREAD_ID": "thread",
        "CLAUDE_CODE_SESSION_ID": "claude",
        "CODEX_SESSION_ID": "codex-session",
        "GEMINI_SESSION_ID": "gemini",
    }
    assert resolve_harness_identity(env) == HarnessIdentity(None, None)


def test_resolvers_agree_on_every_disposition():
    """One environment, both resolvers, same answer (x-b57a acceptance).

    infer_invoking_harness refuses multi-family ambiguity;
    resolve_harness_identity must not answer where it refuses, nor refuse
    where it answers."""
    from fno.dispatch_flags import infer_invoking_harness

    cases = [
        {},  # empty
        {"CODEX_THREAD_ID": "t"},  # single codex marker
        {"CODEX_SESSION_ID": "s", "CODEX_THREAD_ID": "t"},  # one family, two markers
        {"CLAUDE_CODE_SESSION_ID": "c"},  # single claude marker
        {"OPENCODE_SESSION_ID": "ses_1", "GEMINI_SESSION_ID": "g"},  # two families
        {"CODEX_THREAD_ID": "t", "CLAUDE_CODE_SESSION_ID": "c"},  # the leak shape
    ]
    for env in cases:
        inferred = infer_invoking_harness(env)
        resolved = resolve_harness_identity(env)
        if inferred is None:
            assert (resolved.session_id, resolved.harness) == (None, None), env
        else:
            assert resolved.harness == inferred and resolved.session_id, env


def test_whitespace_markers_are_skipped_in_precedence_order():
    env = {
        "CODEX_THREAD_ID": "  ",
        "CLAUDE_CODE_SESSION_ID": "\t",
        "CODEX_SESSION_ID": " codex-session ",
    }
    assert resolve_harness_identity(env) == HarnessIdentity("codex-session", "codex")


def test_no_marker_returns_empty_identity():
    assert resolve_harness_identity({}) == HarnessIdentity(None, None)


# ---- ambiguity-aware owned resolution (AC1-HP / AC2-HP / AC5-CON) --------


def test_present_harness_markers_lists_nonblank_in_precedence_order():
    env = {
        "CODEX_THREAD_ID": "  thread  ",
        "CLAUDE_CODE_SESSION_ID": "",
        "OPENCODE_SESSION_ID": "ses_1",
    }
    assert present_harness_markers(env) == (
        ("CODEX_THREAD_ID", "codex", "thread"),
        ("OPENCODE_SESSION_ID", "opencode", "ses_1"),
    )
    assert present_harness_markers({}) == ()


@pytest.mark.parametrize(
    ("marker", "session_id", "harness"),
    [
        ("CODEX_THREAD_ID", "thread-1", "codex"),
        ("CLAUDE_CODE_SESSION_ID", "claude-1", "claude"),
        ("CODEX_SESSION_ID", "codex-1", "codex"),
        ("GEMINI_SESSION_ID", "gemini-1", "gemini"),
        ("OPENCODE_SESSION_ID", "ses_OpenCode1", "opencode"),
    ],
)
def test_owned_single_marker_matches_precedence(marker, session_id, harness):
    """AC2-HP: exactly one marker -> byte-identical to resolve_harness_identity."""
    env = {marker: session_id}
    owned = resolve_owned_identity(env)
    assert owned.disposition == "single"
    assert owned.session_id == session_id
    assert owned.harness == harness
    assert owned == OwnedHarnessIdentity(
        session_id, harness, ((marker, harness, session_id),), "single"
    )
    # The dominant case must not change the answer precedence gives.
    assert (owned.session_id, owned.harness) == (
        resolve_harness_identity(env).session_id,
        resolve_harness_identity(env).harness,
    )


def test_owned_two_same_family_is_single_not_ambiguous():
    """Two markers of ONE harness (both codex) agree; that is not the bug
    shape. Precedence-first wins, byte-identical to today."""
    env = {"CODEX_THREAD_ID": "thread", "CODEX_SESSION_ID": "session"}
    owned = resolve_owned_identity(env)
    assert owned.disposition == "single"
    assert owned.harness == "codex"
    assert owned.session_id == "thread"


def test_owned_disagreement_degrades_without_proof():
    """AC1-HP core: a claude session with a foreign CODEX_THREAD_ID, and no
    proof available, degrades rather than guessing codex by precedence."""
    env = {"CODEX_THREAD_ID": "foreign", "CLAUDE_CODE_SESSION_ID": "mine"}
    owned = resolve_owned_identity(env)  # no prove/collide injected
    assert owned.disposition == "ambiguous"
    assert owned.session_id is None
    assert owned.harness is None
    assert ("CODEX_THREAD_ID", "codex", "foreign") in owned.markers_present
    assert ("CLAUDE_CODE_SESSION_ID", "claude", "mine") in owned.markers_present


def test_owned_disagreement_prefers_the_proven_marker():
    """AC1-HP: when one marker is provably this process's, the disagreeing set
    resolves to it, not to precedence."""
    env = {"CODEX_THREAD_ID": "foreign", "CLAUDE_CODE_SESSION_ID": "mine"}

    def prove(harness: str, sid: str) -> bool:
        return harness == "claude" and sid == "mine"

    owned = resolve_owned_identity(env, prove=prove)
    assert owned.disposition == "proven"
    assert owned.harness == "claude"
    assert owned.session_id == "mine"


def test_owned_rejects_an_id_another_live_row_owns():
    """AC3-ERR: a candidate owned by a live registry row is rejected and
    recorded, so the claim is never anchored to another worker's session."""
    env = {"CODEX_THREAD_ID": "foreign", "CLAUDE_CODE_SESSION_ID": "mine"}

    def collide(harness: str, sid: str):
        return "x-8224-codex" if sid == "foreign" else None

    def prove(harness: str, sid: str) -> bool:
        return harness == "claude"

    owned = resolve_owned_identity(env, prove=prove, collide=collide)
    assert owned.disposition == "proven"
    assert owned.harness == "claude"
    assert len(owned.rejected) == 1
    assert owned.rejected[0]["session_id"] == "foreign"
    assert owned.rejected[0]["owner"] == "x-8224-codex"
    assert owned.rejected[0]["reason"] == "owned_by_live_row"


def test_owned_multi_family_without_proof_degrades_not_fallback():
    """Two families, no prover: collision-elimination is unsafe without proof
    (it could reject the session's own row when the prover is unavailable and
    leave an inherited foreign marker as the winner), so even when one marker is
    owned by another live row the resolver degrades rather than stamp the other
    by elimination. The collision is still recorded for the event."""
    env = {"CODEX_THREAD_ID": "foreign", "CLAUDE_CODE_SESSION_ID": "mine"}

    def collide(harness: str, sid: str):
        return "owner" if sid == "foreign" else None

    owned = resolve_owned_identity(env, collide=collide)  # no prove
    assert owned.disposition == "ambiguous"
    assert owned.session_id is None
    assert owned.harness is None
    assert owned.rejected[0]["session_id"] == "foreign"


def test_owned_single_family_with_collision_degrades_not_promote_sibling(tmp_path):
    """A same-family collision must not promote an unproven sibling id. Two codex
    markers, one owned by another live row (rejected): without proof, the
    surviving codex marker is unproven and could be foreign, so degrade rather
    than stamp it by elimination."""
    from fno.agents.registry import register_existing_session, row_owning_session_id

    foreign = "019fc87d-ddff-7c90-926a-6bdd7ebb186c"
    reg = tmp_path / "agents.json"
    register_existing_session(provider="codex", session_id=foreign, cwd="/x", registry_path=reg)

    def collide(harness, sid):
        return row_owning_session_id(sid, registry_path=reg)

    env = {"CODEX_THREAD_ID": foreign, "CODEX_SESSION_ID": "sibling-maybe-foreign"}
    owned = resolve_owned_identity(env, collide=collide)  # no prove
    assert owned.disposition == "ambiguous"
    assert owned.session_id is None
    assert owned.harness is None
    assert owned.rejected[0]["session_id"] == foreign


def test_owned_two_unprovable_survivors_degrade():
    """No collision, no proof, two families -> genuinely unknown. Do not pick by
    precedence; None is honest where a stranger's id is the bug."""
    env = {"CODEX_THREAD_ID": "foreign", "CLAUDE_CODE_SESSION_ID": "mine"}
    owned = resolve_owned_identity(env)  # no collide, no prove
    assert owned.disposition == "ambiguous"
    assert owned.session_id is None
    assert owned.harness is None


def test_owned_two_proven_is_still_ambiguous():
    """If both markers were somehow provable (two live identities in one tree),
    do not pick by precedence; record and degrade."""
    env = {"CODEX_THREAD_ID": "a", "CLAUDE_CODE_SESSION_ID": "b"}
    owned = resolve_owned_identity(env, prove=lambda *_: True)
    assert owned.disposition == "ambiguous"
    assert owned.session_id is None


def test_ambient_identity_env_covers_direct_read_markers():
    """AC6-HP scrub coverage: the env list includes the legacy direct-read
    markers (CLAUDECODE_SESSION_ID, HERMES_SESSION_ID), not just the resolver
    tuple, so a module reading a marker directly is scrubbed too."""
    assert "CLAUDECODE_SESSION_ID" in AMBIENT_IDENTITY_ENV
    assert "HERMES_SESSION_ID" in AMBIENT_IDENTITY_ENV
    assert "CODEX_THREAD_ID" in AMBIENT_IDENTITY_ENV
    assert "CLAUDE_CODE_SESSION_ID" in AMBIENT_IDENTITY_ENV
    # Routing vars are NOT identity and must never be swept.
    assert "CLAUDE_CONFIG_DIR" not in AMBIENT_IDENTITY_ENV
    assert "ANTHROPIC_API_KEY" not in AMBIENT_IDENTITY_ENV


# ---- registry collision (AC3-ERR / AC4-ERR) ----------------------------


def _register(tmp_path, session_id, provider="codex", status="live"):
    from fno.agents.registry import register_existing_session

    reg = tmp_path / "agents.json"
    entry = register_existing_session(
        provider=provider, session_id=session_id, cwd="/x", registry_path=reg
    )
    if status != "live":
        import json

        data = json.loads(reg.read_text())
        for row in data.get("agents", []):
            if row.get("name") == entry.name:
                row["status"] = status
        reg.write_text(json.dumps(data))
    return entry.name, reg


def test_row_owning_session_id_finds_live_owner(tmp_path):
    """AC3-ERR: a live row owning a candidate id proves it is not this
    session's; the collider returns the owner's name."""
    from fno.agents.registry import row_owning_session_id

    sid = "019fc87d-ddff-7c90-926a-6bdd7ebb186c"
    name, reg = _register(tmp_path, sid)
    assert row_owning_session_id(sid, registry_path=reg) == name
    # A different id is free.
    assert row_owning_session_id("019fffff-0000-0000-0000-ffffffffffff", registry_path=reg) is None
    # Case-insensitive UUID match.
    assert row_owning_session_id(sid.upper(), registry_path=reg) == name


def test_row_owning_session_id_absent_or_unreadable_is_none(tmp_path):
    """AC4-ERR: an absent or unreadable registry returns None (cannot prove a
    collision) and never raises, so an unreadable registry never blocks init."""
    from fno.agents.registry import row_owning_session_id

    assert row_owning_session_id("019fc87d-...", registry_path=tmp_path / "nope.json") is None
    corrupt = tmp_path / "agents.json"
    corrupt.write_text("{not valid json")
    assert row_owning_session_id("019fc87d-...", registry_path=corrupt) is None
    assert row_owning_session_id("", registry_path=corrupt) is None


def test_row_owning_session_id_exited_row_releases_ownership(tmp_path):
    """An exited/orphaned row no longer owns its id; the id is free to claim."""
    from fno.agents.registry import row_owning_session_id

    sid = "019fc87d-ddff-7c90-926a-6bdd7ebb186c"
    for terminal in ("exited", "orphaned", "permanent_dead"):
        _name, reg = _register(tmp_path, sid, status=terminal)
        assert row_owning_session_id(sid, registry_path=reg) is None


def test_resolve_owned_rejects_a_live_rows_id_via_real_collider(tmp_path):
    """AC3-ERR end-to-end: the owned resolver, wired to the real collider against
    a temp registry, refuses a foreign id a live row owns and records the owner,
    falling through to the claude marker instead of guessing codex."""
    from fno.agents.registry import row_owning_session_id

    foreign = "019fc87d-ddff-7c90-926a-6bdd7ebb186c"
    owner, reg = _register(tmp_path, foreign)

    def collide(harness, sid):
        return row_owning_session_id(sid, registry_path=reg)

    env = {"CODEX_THREAD_ID": foreign, "CLAUDE_CODE_SESSION_ID": "mine"}
    owned = resolve_owned_identity(
        env, prove=lambda h, s: h == "claude", collide=collide
    )
    assert owned.harness == "claude"
    assert owned.session_id == "mine"
    assert len(owned.rejected) == 1
    assert owned.rejected[0]["session_id"] == foreign
    assert owned.rejected[0]["owner"] == owner


def test_owned_proven_marker_wins_over_self_registration_collision(tmp_path):
    """A session that has registered its OWN marker as a live row must not reject
    that marker as a collision. Proof runs before collision, so the proven claude
    marker wins even when a live row owns it (the session's own row), and an
    unregistered foreign CODEX_THREAD_ID never becomes the fallback. This is the
    inverse of the incident: without proof-first, the self-row collision would
    reject claude and leave the foreign marker as the sole survivor."""
    from fno.agents.registry import register_existing_session, row_owning_session_id

    mine = "aaaa1111-mine-mine-mine-aaaaaaaaaaaa"
    foreign = "019fc87d-ddff-7c90-926a-6bdd7ebb186c"
    reg = tmp_path / "agents.json"
    register_existing_session(
        provider="claude", session_id=mine, cwd="/x", registry_path=reg
    )

    def collide(harness, sid):
        return row_owning_session_id(sid, registry_path=reg)

    env = {"CODEX_THREAD_ID": foreign, "CLAUDE_CODE_SESSION_ID": mine}
    owned = resolve_owned_identity(
        env, prove=lambda harness, sid: harness == "claude", collide=collide
    )
    assert owned.disposition == "proven"
    assert owned.harness == "claude"
    assert owned.session_id == mine
    # The session's own row was NOT rejected: proof skipped its collision check.
    assert owned.rejected == ()


def test_owned_lone_foreign_marker_is_not_stamped_when_prover_contradicts():
    """The single-family fast path used to stamp the only marker without any
    ownership check, so a claude hook carrying only an inherited
    CODEX_THREAD_ID recorded the foreign codex identity. A prover that resolves
    to a different harness (False) excludes the lone foreign marker and the
    result degrades to None rather than stamping a stranger's id."""
    env = {"CODEX_THREAD_ID": "foreign"}
    owned = resolve_owned_identity(env, prove=lambda harness, sid: False)
    assert owned.disposition == "ambiguous"
    assert owned.session_id is None
    assert owned.harness is None


def test_owned_distinct_ids_in_one_proven_family_degrade():
    """Two markers of the proven harness with DISTINCT ids (CODEX_THREAD_ID +
    CODEX_SESSION_ID, both codex, different values) cannot be told apart: proof
    is harness-level, not id-level. The resolver degrades rather than pick by
    precedence, which could stamp an inherited same-family stranger id."""
    env = {
        "CODEX_THREAD_ID": "thread",
        "CODEX_SESSION_ID": "session",
        "CLAUDE_CODE_SESSION_ID": "claude-mine",
    }
    owned = resolve_owned_identity(env, prove=lambda harness, sid: harness == "codex")
    assert owned.disposition == "ambiguous"
    assert owned.session_id is None
    assert owned.harness == "codex"  # proven harness kept; only the id degrades


def test_sync_harness_aliases_unknown_suppresses_legacy_backfill():
    """An explicit harness=unknown must not backfill harness_session_id from a
    legacy marker. The manifest records an inherited CODEX_THREAD_ID additively
    for diagnosis; without this guard, _backfill_harness would resurrect it as
    the session's identity, undoing the whole fix."""
    from fno.harness_identity import sync_harness_aliases

    keys = {"claude": "claude_session_id", "codex": "codex_thread_id"}
    data = {
        "harness": "unknown",
        "harness_session_id": None,
        "codex_thread_id": "019fc87d-ddff-7c90-926a-6bdd7ebb186c",
        "claude_session_id": None,
    }
    out = sync_harness_aliases(dict(data), keys)
    assert out["harness_session_id"] in (None, "", "null")
    assert out["codex_thread_id"] == "019fc87d-ddff-7c90-926a-6bdd7ebb186c"

    # A KNOWN harness still backfills from its own legacy key (unchanged).
    known = sync_harness_aliases(
        {"harness": "codex", "harness_session_id": None, "codex_thread_id": "thread"}, keys
    )
    assert known["harness_session_id"] == "thread"


def test_ac7_con_no_resolver_guesses_codex_for_a_disagreement():
    """AC7-CON: three hand-mirrored resolvers (Python, Rust, the bash hook) must
    AGREE that a disagreeing marker set is never silently resolved to codex.

    The Python owned resolver returns no harness for an unprovable disagreement
    (pinned here). The Rust claim writer's ``resolve_harness_from`` returns None
    for the same shape (pinned in claims.rs::
    resolve_harness_single_family_wins_disagreement_is_unknown). The bash hook
    resolves the proven harness via the verb and never codex (pinned in
    tests/hooks/test_init_target_session_id.sh scenario e). The marker tuple
    itself stays identical across Python and Rust (pinned in
    test_rust_marker_mirror_matches_the_python_tuple). No single implementation
    may launder an inherited CODEX_THREAD_ID into ownership.
    """
    env = {"CODEX_THREAD_ID": "foreign", "CLAUDE_CODE_SESSION_ID": "mine"}
    owned = resolve_owned_identity(env)  # no proof, no collision
    assert owned.harness is None
    assert owned.session_id is None
    # A single codex marker is NOT a disagreement and still resolves codex in
    # every implementation (the dominant case, byte-identical across languages).
    assert resolve_owned_identity({"CODEX_THREAD_ID": "thread"}).harness == "codex"





def test_current_session_helpers_refuse_mixed_and_fall_back_to_legacy():
    """One family: precedence winner. Mixed canonical families: the id helper
    refuses like the resolver instead of laundering the claude-legacy marker
    (x-b57a); the ids helper still enumerates every marker, its own job."""
    one_family = {"CODEX_THREAD_ID": " thread ", "CODEX_SESSION_ID": "session"}
    assert current_session_id(one_family) == "thread"
    assert current_session_ids(one_family) == {"thread", "session"}
    mixed = {
        "CODEX_THREAD_ID": " thread ",
        "CLAUDE_CODE_SESSION_ID": "claude",
        "CLAUDE_SESSION_ID": "legacy",
    }
    assert current_session_id(mixed) is None
    assert current_session_ids(mixed) == {"thread", "claude", "legacy"}
    # No canonical marker at all: the pre-migration claude fallback fires.
    assert current_session_id({"CLAUDE_SESSION_ID": " legacy "}) == "legacy"
    assert current_session_ids({}) == set()


def test_ac1_hp_canonical_handle_is_first_eight():
    """The generated mailbox id is the harness's own short-id (first eight),
    carrying no harness prefix (AC1-HP)."""
    assert canonical_handle("019f48e1-5b09-72a0-9bc8-6b364bcf4ae4") == "019f48e1"
    assert canonical_handle("019F48E1-5B09-72A0-9BC8-6B364BCF4AE4") == "019f48e1"
    # ses_ prefix is preserved (case-sensitive); the address includes it, so
    # prefer the full id for ses_ short addressing.
    assert canonical_handle("ses_7f3a9b2cAbCd1234") == "ses_7f3a"


def test_legacy_suffix_handle_is_last_eight_read_only_lookup():
    """The retired last-eight address survives only as a read-only lookup so
    pre-flip handles (e.g. a king addressed 08e8c104) still drain. It is never
    generated for new mail and is NOT the canonical address."""
    assert legacy_suffix_handle("019f48e1-5b09-72a0-9bc8-6b364bcf4ae4") == "4bcf4ae4"
    assert legacy_suffix_handle("019F48E1-5B09-72A0-9BC8-6B364BCF4AE4") == "4bcf4ae4"
    assert legacy_suffix_handle("ses_7f3a9b2cAbCd1234") == "AbCd1234"
    sid = "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4"
    assert legacy_suffix_handle(sid) != canonical_handle(sid)


def test_session_identity_key_normalizes_uuid_case_but_preserves_opencode():
    assert (
        session_identity_key("019F48E1-5B09-72A0-9BC8-6B364BCF4AE4")
        == "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4"
    )
    assert session_identity_key("ses_7f3a9b2cAbCd1234") == "ses_7f3a9b2cAbCd1234"


def test_canonical_handle_takes_no_harness():
    """The signature itself is the documentation: a mailbox id is derived from the
    session id alone. Harness is an envelope attribute and no code path may
    recover it from an address."""
    import inspect

    assert list(inspect.signature(canonical_handle).parameters) == ["session_id"]


def test_ac4_edge_session_id_shorter_than_eight():
    """Boundary: a sub-8-char session id is its own whole handle, never an error."""
    assert canonical_handle("abc") == "abc"
    assert canonical_handle("") == ""


@pytest.mark.parametrize("provider", ["claude", "codex", "gemini", "agy", "opencode"])
def test_legacy_handle_re_matches_every_retired_provider(provider):
    """The pattern recognizes every retired provider address so callers can
    refuse or report it by name - never accept one."""
    assert LEGACY_HANDLE_RE.fullmatch(f"{provider}-019f48e1")


def test_legacy_handle_re_rejects_non_retired_shapes():
    assert not LEGACY_HANDLE_RE.match("019f48e1")  # the real address
    assert not LEGACY_HANDLE_RE.match("fno-019f48e1")  # friendly project alias
    assert not LEGACY_HANDLE_RE.match("tgt-node-claude-g1")  # a mesh name


def test_ac1_fr_registry_name_equals_canonical_handle(tmp_path):
    """The registry row name a session registers under MUST equal the handle a
    sender resolves and the drain reads, or a queued message strands. Assert the
    registry derives its name via the same shared function (drift fails CI)."""
    from fno.agents.registry import register_existing_session

    sid = "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4"
    entry = register_existing_session(
        provider="codex",
        session_id=sid,
        cwd="/tmp",
        registry_path=tmp_path / "agents.json",
    )
    assert entry.name == canonical_handle(sid) == "019f48e1"


def test_no_generating_surface_produces_a_retired_address(tmp_path, monkeypatch):
    """Every surface that MINTS an address must produce the bare form.

    The retired `<harness>-<short8>` is refused on the read side, but a refusal is
    only a backstop - the real fix is that nothing mints one. That was prose in a
    commit message until this test; now a reintroduced generator fails CI instead
    of quietly writing mail nothing can drain.
    """
    from fno.agents.registry import register_existing_session
    from fno.agents.self_stamp import stamp_from
    from fno.mail.envelope import wrap_fno_mail
    from fno.paths_testing import use_tmpdir

    use_tmpdir(monkeypatch, tmp_path)
    sid = "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4"
    for marker, _ in HARNESS_SESSION_MARKERS:
        monkeypatch.delenv(marker, raising=False)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", sid)

    minted = [
        canonical_handle(sid),
        stamp_from(None),
        register_existing_session(
            provider="claude", session_id=sid, cwd="/tmp",
            registry_path=tmp_path / "agents.json",
        ).name,
    ]
    for value in minted:
        assert not LEGACY_HANDLE_RE.match(value), f"{value!r} is a retired address"

    # The wire envelope's from/to too - the bus columns drifted from this once.
    body = wrap_fno_mail("hi", from_=stamp_from(None), harness="claude-code",
                         model="m", to=canonical_handle(sid))
    assert 'from="019f48e1"' in body and 'to="019f48e1"' in body


def test_ac4_err_legacy_suffix_is_read_only_compatibility_lookup():
    """Legacy last-eight lookup is explicit read-only compatibility for in-flight
    pre-flip handles, never generation."""
    from fno import harness_identity

    sid = "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4"
    assert harness_identity.legacy_suffix_handle(sid) == "4bcf4ae4"
    assert harness_identity.legacy_suffix_handle(sid) != canonical_handle(sid)


def test_claude_transport_key_equals_canonical_address_since_flip():
    """claude_transport_short_id is claude's own first-eight job key. Since the
    2026-08-10 flip made the harness's own short-id the mailbox address, it is
    equal in value to canonical_handle; kept as the named seam for claude's
    native job key at claude-specific call sites."""
    sid = "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4"
    assert claude_transport_short_id(sid) == "019f48e1"
    assert claude_transport_short_id(sid) == canonical_handle(sid)


def test_rust_marker_mirror_matches_the_python_tuple():
    """The Rust claim writer's marker copy must equal HARNESS_SESSION_MARKERS.

    claims.rs carries a hand-maintained mirror because it cannot import Python,
    and its own doc comment promises the Rust writer tags a claim with the same
    harness the Python resolver would. Nothing enforced that promise: a marker
    added to the tuple leaves the Rust writer silently tagging the older harness,
    and a Python/Rust disagreement on ambient identity is the shape that already
    produced false-STALE claims once.

    Names AND order are compared: the tuple is a precedence list, so two copies
    holding the same names in a different order still resolve differently when a
    session exports more than one marker.
    """
    import re
    from pathlib import Path

    claims_rs = (
        Path(__file__).resolve().parents[3] / "crates" / "fno-agents" / "src" / "claims.rs"
    )
    source = claims_rs.read_text()
    block = re.search(
        r"const HARNESS_SESSION_MARKERS:[^=]*=\s*&\[(.*?)\];", source, re.S
    )
    assert block, f"no HARNESS_SESSION_MARKERS const found in {claims_rs}"
    # Drop commented-out lines first. Matching raw source counts a
    # `// ("MARKER", "harness"),` as an entry, so commenting a marker out - which
    # removes it from the compiled array - would leave this green.
    entries = "\n".join(
        line for line in block.group(1).splitlines() if not line.lstrip().startswith("//")
    )
    mirrored = re.findall(r'\(\s*"([^"]+)"\s*,\s*"([^"]+)"\s*\)', entries)

    assert mirrored == list(HARNESS_SESSION_MARKERS), (
        "claims.rs HARNESS_SESSION_MARKERS drifted from harness_identity.py; "
        f"rust={mirrored} python={list(HARNESS_SESSION_MARKERS)}"
    )


def test_rust_identity_mirror_matches_python_addressing_rule():
    """identity.rs mirrors the Python addressing rule because the Rust lifecycle
    client cannot import Python, and a divergence strands mail silently: a
    durable send addresses one handle while its recipient drains another.

    Pins the load-bearing facts in the Rust source: canonical_handle takes
    the FIRST eight chars, the ses_ branch preserves case while the UUID branch
    lowercases (matching Python's session_identity_key), and the tier order is
    [full, canonical(first-8), legacy_suffix(last-8)]. A Python-only or Rust-only
    change to the addressing rule fails here rather than shipping a silent
    parity break.
    """
    import re
    from pathlib import Path

    identity_rs = (
        Path(__file__).resolve().parents[3] / "crates" / "fno-agents" / "src" / "identity.rs"
    )
    source = identity_rs.read_text()

    # Each function body runs to its column-0 close brace; inner braces are
    # indented, so the first "\n}" bounds the function.
    canonical_body = re.search(r"fn canonical_handle.*?\n\}", source, re.S)
    assert canonical_body, "no canonical_handle fn in identity.rs"
    assert ".chars().take(8)" in canonical_body.group(0), (
        "Rust canonical_handle must take the FIRST eight chars to match Python"
    )
    # Case rule: Python's session_identity_key preserves ses_ case and
    # lowercases the UUID family. Rust must mirror both branches or a mixed-case
    # ses_ id (ses_AbCd...) addresses one handle in Python and another in Rust.
    assert 'starts_with("ses_")' in canonical_body.group(0), (
        "Rust canonical_handle must preserve ses_ case (the ses_ branch) to match Python"
    )
    assert "to_ascii_lowercase" in canonical_body.group(0), (
        "Rust canonical_handle must lowercase the non-ses_ branch to match Python"
    )

    suffix_body = re.search(r"fn legacy_suffix_handle.*?\n\}", source, re.S)
    assert suffix_body, "no legacy_suffix_handle fn in identity.rs"
    assert ".rev().take(8)" in suffix_body.group(0), (
        "Rust legacy_suffix_handle must take the LAST eight chars to match Python"
    )

    assert re.search(
        r"session_id\.to_string\(\),\s*canonical_handle\(session_id\),\s*"
        r"legacy_suffix_handle\(session_id\)",
        source,
    ), "Rust tier order must be [full, canonical(first-8), legacy_suffix(last-8)]"


def test_strip_flags_keep_fno_plumbing():
    """The self-rescue strip set is foreign HARNESS families only. TARGET_SESSION_ID
    is this run's own claim linkage (the resolver never consults it), so a strip
    line that includes it would tell the operator to break their own run's claim
    matching while curing nothing."""
    from fno.harness_identity import ambient_identity_strip_flags

    env = {
        "CLAUDE_CODE_SESSION_ID": "own",
        "CODEX_THREAD_ID": "foreign",
        "CODEX_CI": "1",
        "TARGET_SESSION_ID": "my-run",
    }
    flags = ambient_identity_strip_flags("claude", env)
    assert "CODEX_THREAD_ID" in flags and "CODEX_CI" in flags
    assert "TARGET_SESSION_ID" not in flags
    assert "CLAUDE_CODE_SESSION_ID" not in flags


def test_strip_flags_skip_foreign_name_with_matching_keep_session_id():
    from fno.harness_identity import ambient_identity_strip_flags

    env = {
        "CLAUDE_CODE_SESSION_ID": "own-session",
        "CODEX_COMPANION_SESSION_ID": "own-session",
    }

    assert ambient_identity_strip_flags("claude", env) == []


def test_strip_flags_keep_equal_foreign_resolver_marker_for_ambiguity_remedy():
    from fno.harness_identity import ambient_identity_strip_flags

    env = {
        "CLAUDE_CODE_SESSION_ID": "own-session",
        "CODEX_THREAD_ID": "own-session",
    }

    assert ambient_identity_strip_flags("claude", env) == ["-u", "CODEX_THREAD_ID"]


def test_strip_flags_keep_foreign_name_with_different_session_id():
    from fno.harness_identity import ambient_identity_strip_flags

    env = {
        "CLAUDE_CODE_SESSION_ID": "own-session",
        "CODEX_COMPANION_SESSION_ID": "foreign-session",
    }

    assert ambient_identity_strip_flags("claude", env) == [
        "-u",
        "CODEX_COMPANION_SESSION_ID",
    ]


# --- attester identity: bound to the emitting process -------------------------


def test_attester_witness_process_on_equal_family_ancestor():
    from fno.harness_identity import _attester_witness

    # A NON-family carrier agreeing first (the fno CLI process carrying the
    # inherited marker) still corroborates only through the family carrier
    # behind it; here the family carrier agrees too.
    assert (
        _attester_witness("CODEX_THREAD_ID", "sess-a", [None, "sess-a"], [False, True])
        == "process"
    )


def test_attester_witness_env_only_when_no_family_carrier():
    from fno.harness_identity import _attester_witness

    # Carriers that agree but are not family processes (shells, the CLI
    # python) corroborate nothing on their own: no family carrier, env_only -
    # the daemon-carrier lane must degrade, never wedge or over-certify.
    assert (
        _attester_witness("CODEX_THREAD_ID", "sess-a", ["sess-a", None], [False, False])
        == "env_only"
    )
    assert _attester_witness("CODEX_THREAD_ID", "sess-a", [None, None], [False, False]) == "env_only"
    assert _attester_witness("CODEX_THREAD_ID", "sess-a", [], []) == "env_only"


def test_attester_witness_raises_on_differing_family_ancestor():
    from fno.harness_identity import AttesterIdentityConflict, _attester_witness

    with pytest.raises(AttesterIdentityConflict) as exc:
        _attester_witness("CODEX_THREAD_ID", "sess-forged", ["sess-true"], [True])
    # Both ids are named: the refusal is the one place a reader can see which
    # session the harness says versus which the env claims.
    assert "sess-true" in str(exc.value)
    assert "sess-forged" in str(exc.value)


def test_attester_witness_raise_outranks_the_equal_match():
    """The override shape yields BOTH an equal carrier (the shell carrying the
    assignment, not a family process) and a differing family carrier (the
    harness above it, the only kind that can mint the id). Stopping at the
    first equal carrier would certify the forgery, so the family carrier's
    disagreement raises."""
    from fno.harness_identity import AttesterIdentityConflict, _attester_witness

    with pytest.raises(AttesterIdentityConflict):
        _attester_witness(
            "CODEX_THREAD_ID",
            "sess-forged",
            ["sess-forged", None, "sess-true"],
            [False, False, True],
        )


def test_a_stale_marker_above_the_session_never_vetoes():
    """The daemon-carrier lane: the session process (a family carrier) agrees
    with the env, and a LONG-LIVED ancestor above it retains a previous
    session's marker. The nearer family carrier decided; the stale value above
    was never the minter of this id, so the witness is process, not a refusal
    that would wedge every emit under that daemon."""
    from fno.harness_identity import _attester_witness

    assert (
        _attester_witness(
            "CODEX_THREAD_ID",
            "sess-now",
            ["sess-now", "sess-old"],
            [True, True],
        )
        == "process"
    )


def test_resolve_attester_identity_reads_winning_marker_and_empty_without_one():
    """Ancestry-adaptive, for the same reason the actor test is: run under a
    real codex session, a family ancestor carries a DIFFERENT live id and the
    resolver raises - the refusal is correct there, so this asserts it rather
    than erroring. On any other host no codex ancestor exists and the honest
    answer is env_only."""
    from fno.harness_identity import AttesterIdentityConflict, resolve_attester_identity

    try:
        assert resolve_attester_identity({"CODEX_SESSION_ID": "codex-sess"}) == (
            "codex-sess",
            "env_only",
        )
    except AttesterIdentityConflict:
        pass  # a live codex ancestry disagrees; the refusal is the verdict
    assert resolve_attester_identity({}) == ("", "env_only")


def test_resolve_attester_identity_refuses_mixed_family_env():
    """Markers from two harness families: one is foreign and inherited. Empty
    rather than by precedence - picking either would launder or ignore a
    provably mixed env."""
    from fno.harness_identity import resolve_attester_identity

    env = {"CODEX_THREAD_ID": "codex-id", "CLAUDE_CODE_SESSION_ID": "claude-id"}
    assert resolve_attester_identity(env) == ("", "env_only")


def _proc_environ_strictly_readable() -> bool:
    """Whether this OS GUARANTEES reading an ancestor's environment: linux
    /proc does. darwin's `ps eww` is PARTIAL - it exposed the harness chain's
    markers in a live session yet showed nothing for other processes - so the
    darwin arms below accept the recorded outcome rather than demand one."""
    return Path("/proc/self/environ").exists()


def test_resolve_attester_identity_refuses_the_command_line_override():
    """The verbatim reproduction: a command-scoped `CODEX_THREAD_ID=<foreign>`
    assignment makes the emitting env disagree with the bash parent that
    inherited the harness's own value. The resolver must raise, naming both
    ids. Runs live where the OS exposes ancestor environments (linux CI); on
    darwin the same chain honestly resolves env_only, which that arm pins."""
    import subprocess
    import sys

    probe = (
        "from fno.harness_identity import AttesterIdentityConflict, "
        "resolve_attester_identity\n"
        "try:\n"
        "    print(resolve_attester_identity())\n"
        "except AttesterIdentityConflict as exc:\n"
        "    print('CONFLICT', exc)\n"
        "    raise SystemExit(3)\n"
    )
    import fno as _fno

    src_dir = str(Path(_fno.__file__).resolve().parents[1])
    # The parent must be a FAMILY carrier (a process whose argv0 names the
    # marker's harness), because the nearest family carrier is what decides:
    # `exec -a codex` renames the intermediate bash, so its argv0 carries the
    # family token while its env still holds the harness's own id. The `rc=$?
    # ... exit $rc` tail is load-bearing: a lone command gets tail-exec'd
    # (the leaf REPLACES the renamed bash and inherits its pid), leaving the
    # walk no family carrier at all - the pre-tail shape passed only via
    # full-argv matching, the exact artifact the argv0 rule removed. The tail
    # also re-exits with the leaf's status; a bare `; :` would swallow it.
    import shlex

    leaf = (
        f"CODEX_THREAD_ID=foreign-sess {sys.executable} -c "
        "'import os; exec(os.environ[\"FNO_IDENTITY_PROBE\"])'; "
        "rc=$?; :; exit \"$rc\""
    )
    wrapped = "exec -a codex bash -c " + shlex.quote(leaf)
    r = subprocess.run(
        ["bash", "-c", wrapped],
        env={
            **os.environ,
            "CODEX_THREAD_ID": "true-sess",
            "PYTHONPATH": src_dir,
            "FNO_IDENTITY_PROBE": probe,
        },
        capture_output=True,
        text=True,
    )
    if _proc_environ_strictly_readable():
        # linux: /proc answers for every ancestor, so the family carrier is
        # readable and its disagreement with the leaf's override must raise.
        assert r.returncode == 3, r.stdout + r.stderr
        assert "true-sess" in r.stdout and "foreign-sess" in r.stdout
    else:
        # darwin: `ps eww` readability is partial by measurement, so the
        # carrier is either readable (the disagreement raises) or not
        # (env_only). Both are honest. The one forbidden outcome on every
        # OS is the forged stamp: the override resolving with a `process`
        # witness, which is what full-argv family matching shipped.
        if r.returncode == 0:
            assert "env_only" in r.stdout, r.stdout + r.stderr
        else:
            assert r.returncode == 3, r.stdout + r.stderr
            assert "true-sess" in r.stdout and "foreign-sess" in r.stdout


def test_resolve_attester_identity_corroborates_the_own_id():
    """The other direction, both positive: the same chain with the harness's
    OWN id emits nothing conflicting - process witness on linux, env_only on
    darwin - and never raises against a session's own marker."""
    import subprocess
    import sys

    probe = (
        "from fno.harness_identity import resolve_attester_identity\n"
        "sid, witness = resolve_attester_identity()\n"
        "print(sid, witness)\n"
    )
    import fno as _fno

    src_dir = str(Path(_fno.__file__).resolve().parents[1])
    # Same family-carrier parent as the override test (exec -a codex renames
    # the intermediate bash; the rc-capturing tail keeps it alive under the
    # leaf): the carrier agrees with the env. linux reads it and answers
    # process; darwin cannot read a fresh child's environment, so the same
    # chain honestly degrades to env_only there.
    import shlex

    leaf = (
        f"{sys.executable} -c "
        "'import os; exec(os.environ[\"FNO_IDENTITY_PROBE\"])'; "
        "rc=$?; :; exit \"$rc\""
    )
    wrapped = "exec -a codex bash -c " + shlex.quote(leaf)
    r = subprocess.run(
        ["bash", "-c", wrapped],
        env={
            **os.environ,
            "CODEX_THREAD_ID": "sess-own",
            "PYTHONPATH": src_dir,
            "FNO_IDENTITY_PROBE": probe,
        },
        capture_output=True,
        text=True,
    )
    assert r.returncode == 0, r.stdout + r.stderr
    sid, witness = r.stdout.split()
    assert sid == "sess-own"
    if _proc_environ_strictly_readable():
        assert witness == "process"
    else:
        # darwin: ps eww readability is partial, so the agreeing carrier is
        # corroborated where it was readable (process) and honestly
        # env_only where it was not. Either is correct; a raise is not.
        assert witness in ("process", "env_only")
