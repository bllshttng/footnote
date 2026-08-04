"""Tests for shared ambient harness session identity resolution."""

from __future__ import annotations

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
    env = {
        "CODEX_THREAD_ID": "thread",
        "CLAUDE_CODE_SESSION_ID": "claude",
        "CODEX_SESSION_ID": "codex-session",
        "GEMINI_SESSION_ID": "gemini",
    }
    assert resolve_harness_identity(env) == HarnessIdentity("thread", "codex")


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





def test_current_session_helpers_share_precedence_and_legacy_fallback():
    env = {
        "CODEX_THREAD_ID": " thread ",
        "CLAUDE_CODE_SESSION_ID": "claude",
        "CLAUDE_SESSION_ID": "legacy",
    }
    assert current_session_id(env) == "thread"
    assert current_session_ids(env) == {"thread", "claude", "legacy"}
    assert current_session_id({"CLAUDE_SESSION_ID": " legacy "}) == "legacy"
    assert current_session_ids({}) == set()


def test_ac1_hp_canonical_handle_is_random_tail():
    """The generated mailbox id carries no harness prefix (AC1-HP)."""
    assert canonical_handle("019f48e1-5b09-72a0-9bc8-6b364bcf4ae4") == "4bcf4ae4"
    assert canonical_handle("019F48E1-5B09-72A0-9BC8-6B364BCF4AE4") == "4bcf4ae4"
    assert canonical_handle("ses_7f3a9b2cAbCd1234") == "AbCd1234"


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
    assert entry.name == canonical_handle(sid) == "4bcf4ae4"


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
    assert 'from="4bcf4ae4"' in body and 'to="4bcf4ae4"' in body


def test_ac4_err_legacy_prefix_has_one_named_compatibility_owner():
    """Legacy first-eight lookup is explicit compatibility, never generation."""
    from fno import harness_identity

    sid = "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4"
    assert harness_identity.legacy_prefix_handle(sid) == "019f48e1"
    assert harness_identity.legacy_prefix_handle(sid) != canonical_handle(sid)


def test_claude_transport_key_is_named_separately_from_mailbox_address():
    sid = "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4"
    assert claude_transport_short_id(sid) == "019f48e1"
    assert claude_transport_short_id(sid) != canonical_handle(sid)


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
