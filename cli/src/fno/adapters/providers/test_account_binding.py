"""The shared effective-account binding (x-d6be).

Every test here answers one question: does the binding name the account whose
credential a launch will ACTUALLY read, rather than the account a path or a
stamp says should be there?
"""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.adapters.providers import binding, managed
from fno.adapters.providers.model import ProviderRecord

MAKERS = {"account_uuid": "acc-makers", "organization_uuid": "org-1", "email": "makers@x"}
READYRULE = {
    "account_uuid": "acc-readyrule",
    "organization_uuid": "org-1",
    "email": "readyrule@x",
}


def _blob(token: str) -> str:
    return '{"claudeAiOauth": {"accessToken": "%s"}}' % token


def _record(rid: str, **kw) -> ProviderRecord:
    kw.setdefault("harness", "claude")
    kw.setdefault("auth", "managed")
    return ProviderRecord(id=rid, name=rid, **kw)


@pytest.fixture
def store(tmp_path: Path) -> Path:
    root = tmp_path / "providers"
    root.mkdir()
    return root


def _bind(root: Path, rid: str, principal: dict) -> None:
    managed.write_record_principal(rid, principal, root)


def _serve(monkeypatch, mapping: dict[str, dict]) -> list[str]:
    """Answer the profile endpoint from ``token -> principal``, offline.

    Returns the list of tokens actually proven, so a test can assert a probe
    was skipped rather than infer it from a status that has other causes.
    """
    proven: list[str] = []

    def fake(blob):
        proven.append(blob)
        token = blob.split('"accessToken": "')[-1].split('"')[0] if blob else ""
        principal = mapping.get(token)
        return (principal, None) if principal else (None, "profile-unavailable")

    monkeypatch.setattr(managed, "slot_principal", fake)
    return proven


# --- credential_root: one owner for "which dir serves this record" -----------


def test_credential_root_prefers_config_dir_then_source_then_slot(tmp_path: Path) -> None:
    both = _record(
        "both",
        auth="oauth_dir",
        config_dir=tmp_path / "own",
        credentials_source=tmp_path / "staged",
    )
    assert binding.credential_root(both) == tmp_path / "own"
    staged = _record("staged", auth="oauth_dir", credentials_source=tmp_path / "staged")
    assert binding.credential_root(staged) == tmp_path / "staged"
    assert binding.credential_root(_record("shared")) is None


# --- AC1-HP: the live principal outranks the stamp ---------------------------


def test_unpinned_slot_names_the_live_account_not_the_stamp(
    store: Path, monkeypatch
) -> None:
    """AC1-HP: the stamp says makers; the slot serves readyrule. Name readyrule."""
    managed.stamp_active_slot("claude", "makers", store)
    _bind(store, "makers", MAKERS)
    _bind(store, "readyrule", READYRULE)
    _serve(monkeypatch, {"t-readyrule": READYRULE})
    monkeypatch.setattr(binding, "credential_blobs", lambda *_: [_blob("t-readyrule")])

    got = binding.resolve_account_binding(
        None, root=store, by_id={"makers": _record("makers"), "readyrule": _record("readyrule")}
    )

    assert got.status == binding.MATCHED
    assert got.matched_record == "readyrule"
    assert got.observed_principal == "acc-readyrule/org-1"
    assert got.credential_ref == managed.credential_digest(_blob("t-readyrule"))


def test_unpinned_slot_reports_ambiguity_when_two_records_claim_the_identity(
    store: Path, monkeypatch
) -> None:
    """AC1-HP: matching is not unique, so say so rather than pick the first."""
    _bind(store, "a", MAKERS)
    _bind(store, "b", MAKERS)
    _serve(monkeypatch, {"t-makers": MAKERS})
    monkeypatch.setattr(binding, "credential_blobs", lambda *_: [_blob("t-makers")])

    got = binding.resolve_account_binding(
        None, root=store, by_id={"a": _record("a"), "b": _record("b")}
    )

    assert got.status == binding.AMBIGUOUS
    assert got.reason == "ambiguous-match"
    assert got.matched_record is None


def test_pinned_record_whose_root_serves_another_account_is_a_mismatch(
    store: Path, monkeypatch, tmp_path: Path
) -> None:
    """AC2-ERR's evidence: a makers pin over a root that serves readyrule."""
    _bind(store, "makers", MAKERS)
    _serve(monkeypatch, {"t-readyrule": READYRULE})
    monkeypatch.setattr(binding, "credential_blobs", lambda *_: [_blob("t-readyrule")])
    record = _record("makers", config_dir=tmp_path / ".claude-alt")

    got = binding.resolve_account_binding(record, root=store, by_id={"makers": record})

    assert got.status == binding.MISMATCH
    assert got.observed_label == "readyrule@x"
    assert binding.MISMATCH_RECEIPT in got.receipt
    assert not got.ok


def test_unbound_record_is_unknown_without_a_profile_call(
    store: Path, monkeypatch, tmp_path: Path
) -> None:
    """No reference identity means nothing to compare, and nothing to pay for."""
    proven = _serve(monkeypatch, {})
    monkeypatch.setattr(binding, "credential_blobs", lambda *_: [_blob("t-makers")])
    record = _record("makers", config_dir=tmp_path / ".claude-alt")

    got = binding.resolve_account_binding(record, root=store, by_id={"makers": record})

    assert got.status == binding.UNKNOWN
    assert got.reason == "unbound-principal"
    assert proven == []
    assert binding.UNKNOWN_RECEIPT in got.receipt


def test_receipt_never_names_an_account_when_identity_is_unknown(store: Path) -> None:
    """AC3-EDGE: an unknown binding prints its cause, never a served account."""
    got = binding.resolve_account_binding(
        _record("makers"), root=store, by_id={"makers": _record("makers")}
    )
    assert got.status == binding.UNKNOWN
    assert "makers" not in got.receipt
    assert binding.UNKNOWN_RECEIPT in got.receipt


def test_a_slot_holding_two_accounts_is_ambiguous_not_a_match(
    store: Path, monkeypatch
) -> None:
    _bind(store, "makers", MAKERS)
    _serve(monkeypatch, {"t-makers": MAKERS, "t-readyrule": READYRULE})
    monkeypatch.setattr(
        binding, "credential_blobs", lambda *_: [_blob("t-makers"), _blob("t-readyrule")]
    )
    record = _record("makers")

    got = binding.resolve_account_binding(record, root=store, by_id={"makers": record})

    assert got.status == binding.AMBIGUOUS
    assert got.reason == "ambiguous-slot"


def test_unreadable_credential_source_is_unknown_never_a_pass(
    store: Path, monkeypatch
) -> None:
    _bind(store, "makers", MAKERS)

    def boom(*_a, **_k):
        raise managed.KeychainError("denied")

    monkeypatch.setattr(binding, "credential_blobs", boom)
    record = _record("makers")

    got = binding.resolve_account_binding(record, root=store, by_id={"makers": record})

    assert got.status == binding.UNKNOWN
    assert got.reason == "credential-unreadable"


def test_api_key_record_is_not_an_ambient_subscription_question(store: Path) -> None:
    record = _record("routed", auth="api_key", env={"ANTHROPIC_API_KEY": "k"})
    got = binding.resolve_account_binding(record, root=store, by_id={"routed": record})
    assert got.status == binding.UNKNOWN
    assert got.reason == "api-key-route"


# --- AC1-EDGE: shared transcripts, and a credential change inside the TTL ----


def test_shared_transcript_folders_do_not_merge_credential_identity(
    store: Path, monkeypatch, tmp_path: Path
) -> None:
    """AC1-EDGE: two roots symlink the same transcripts; identity stays distinct."""
    canonical = tmp_path / ".claude"
    alt = tmp_path / ".claude-alt"
    for root in (canonical, alt):
        root.mkdir()
    (canonical / "projects").mkdir()
    (alt / "projects").symlink_to(canonical / "projects")
    (canonical / ".credentials.json").write_text(_blob("t-makers"))
    (alt / ".credentials.json").write_text(_blob("t-readyrule"))
    monkeypatch.setattr(managed, "_read_claude_keychain_item", lambda _s: None)

    assert binding.credential_blobs("claude", canonical) == [_blob("t-makers")]
    assert binding.credential_blobs("claude", alt) == [_blob("t-readyrule")]


def test_a_credential_change_invalidates_cached_evidence_inside_the_ttl(
    store: Path, monkeypatch
) -> None:
    """AC1-EDGE: the cache is keyed on the credential, not on the clock."""
    _bind(store, "makers", MAKERS)
    record = _record("makers")
    proven = _serve(monkeypatch, {"t-makers": MAKERS, "t-readyrule": READYRULE})

    monkeypatch.setattr(binding, "credential_blobs", lambda *_: [_blob("t-makers")])
    first = binding.resolve_account_binding(
        record, root=store, by_id={"makers": record}, now=100.0
    )
    assert first.status == binding.MATCHED
    # Same credential, well inside the TTL: served from cache, no second proof.
    binding.resolve_account_binding(record, root=store, by_id={"makers": record}, now=110.0)
    assert len(proven) == 1

    # The account signs out and back in as readyrule. The clock has barely
    # moved, so only the credential key can catch this.
    monkeypatch.setattr(binding, "credential_blobs", lambda *_: [_blob("t-readyrule")])
    after = binding.resolve_account_binding(
        record, root=store, by_id={"makers": record}, now=120.0
    )

    assert after.status == binding.MISMATCH
    assert after.observed_principal == "acc-readyrule/org-1"
    assert len(proven) == 2


# --- the bearer lane: what a usage probe asks --------------------------------


def test_bearer_lane_defers_to_the_per_bearer_verdict(
    store: Path, monkeypatch, tmp_path: Path
) -> None:
    record = _record("makers", config_dir=tmp_path / ".claude-alt")
    _bind(store, "makers", MAKERS)
    seen: list[str] = []

    def verdict(cli, rid, root, bearer, **kw):
        seen.append(bearer)
        return "mismatch"

    monkeypatch.setattr(managed, "bearer_principal_verdict", verdict)

    got = binding.resolve_account_binding(
        record, root=store, bearer="tok-abc", by_id={"makers": record}
    )

    assert seen == ["tok-abc"]
    assert got.status == binding.MISMATCH
    assert got.credential_ref == managed.credential_digest("tok-abc")


def test_bearer_lane_refuses_a_shared_slot_holding_two_credentials(
    store: Path, monkeypatch
) -> None:
    record = _record("makers")
    _bind(store, "makers", MAKERS)
    monkeypatch.setattr(
        managed, "canonical_slot_blobs", lambda _c: [_blob("a"), _blob("b")]
    )
    monkeypatch.setattr(
        managed,
        "bearer_principal_verdict",
        lambda *a, **k: pytest.fail("a two-credential slot must settle offline"),
    )

    got = binding.resolve_account_binding(
        record, root=store, bearer="tok-abc", by_id={"makers": record}
    )

    assert got.status == binding.AMBIGUOUS
    assert got.reason == "ambiguous-slot"


def test_a_cached_binding_reports_when_it_was_proven(store: Path, monkeypatch) -> None:
    """The read time is not the observation time. Reporting it as one renders
    every cached answer as brand new, however old the proof behind it is."""
    _bind(store, "makers", MAKERS)
    record = _record("makers")
    _serve(monkeypatch, {"t-makers": MAKERS})
    monkeypatch.setattr(binding, "credential_blobs", lambda *_: [_blob("t-makers")])

    fresh = binding.resolve_account_binding(
        record, root=store, by_id={"makers": record}, now=100.0
    )
    cached = binding.resolve_account_binding(
        record, root=store, by_id={"makers": record}, now=400.0
    )

    assert fresh.observed_at == 100.0
    assert cached.status == binding.MATCHED
    assert cached.observed_at == 100.0  # not 400.0: the proof is 5 minutes old
