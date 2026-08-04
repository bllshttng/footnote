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
        session_id, harness, ((marker, harness),), "single"
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
    assert ("CODEX_THREAD_ID", "codex") in owned.markers_present
    assert ("CLAUDE_CODE_SESSION_ID", "claude") in owned.markers_present


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


def test_owned_rejected_and_unproven_degrades_to_ambiguous():
    """AC3-ERR + degrade: codex rejected (owned elsewhere), claude unproven ->
    no guess; None is honest, a stranger's id is the bug."""
    env = {"CODEX_THREAD_ID": "foreign", "CLAUDE_CODE_SESSION_ID": "mine"}

    def collide(harness: str, sid: str):
        return "owner" if sid == "foreign" else None

    owned = resolve_owned_identity(env, collide=collide)  # no prove
    assert owned.disposition == "ambiguous"
    assert owned.session_id is None
    assert owned.rejected[0]["session_id"] == "foreign"


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
