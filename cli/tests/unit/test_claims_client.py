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
