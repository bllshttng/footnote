"""The Python claim API delegates ownership decisions to fno-agents."""
from __future__ import annotations

import os

from fno.claims import core


def _claim_payload(key: str) -> dict:
    return {"schema_version": 1, "key": key, "holder": "holder-1", "acquired_at": 1_700_000_000_000, "pid": 42, "host": "test-host", "outcome": "acquired"}


def test_acquire_claim_uses_native_json_door(monkeypatch) -> None:
    calls = []

    def fake_native(operation, key, flags):
        calls.append((operation, key, flags))
        return _claim_payload(key)

    monkeypatch.setattr(core, "_native_claim", fake_native)
    claim = core.acquire_claim("node:x-test", "holder-1", ttl_ms=60_000, reason="test")
    assert claim.key == "node:x-test"
    assert calls == [("acquire", "node:x-test", ["--holder", "holder-1", "--ttl-ms", "60000", "--reason", "test", "--pid", str(os.getpid())])]


def test_claim_status_uses_native_json_door(monkeypatch) -> None:
    calls = []

    def fake_native(operation, key, flags):
        calls.append((operation, key, flags))
        return {"key": key, "state": "free"}

    monkeypatch.setattr(core, "_native_claim", fake_native)
    assert core.claim_status("node:x-test") == {"key": "node:x-test", "state": "free"}
    assert calls == [("status", "node:x-test", [])]


def test_release_claim_requires_native_confirmation_of_unlink(monkeypatch) -> None:
    calls = []

    def fake_native(operation, key, flags):
        calls.append(operation)
        if operation == "status":
            return {**_claim_payload(key), "state": "live"}
        return {"outcome": "not_released", "released": False}

    monkeypatch.setattr(core, "_legacy_claim_call", lambda _key, _root: False)
    monkeypatch.setattr(core, "_native_claim", fake_native)
    assert core.release_claim("node:x-test", "holder-1") is None
    assert calls == ["release"]


def test_release_claim_returns_the_claim_native_unlinked(monkeypatch) -> None:
    key = "node:x-test"
    released = {**_claim_payload(key), "acquired_at": 1_800_000_000_000}

    def fake_native(operation, _key, _flags):
        return {"outcome": "released", "released": True, "claim": released}

    monkeypatch.setattr(core, "_legacy_claim_call", lambda _key, _root: False)
    monkeypatch.setattr(core, "_native_claim", fake_native)
    claim = core.release_claim(key, "holder-1")
    assert claim is not None
    assert claim.acquired_at == released["acquired_at"]
