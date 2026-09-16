"""The Python claim API delegates ownership decisions to fno-agents."""
from __future__ import annotations

import os
from pathlib import Path

from fno.claims import core


def _claim_payload(key: str = "node:x-test") -> dict:
    return {
        "schema_version": 1,
        "key": key,
        "holder": "holder-1",
        "acquired_at": 1_700_000_000_000,
        "pid": 42,
        "host": "test-host",
        "outcome": "acquired",
    }


def test_acquire_claim_uses_native_json_door(monkeypatch, tmp_path: Path) -> None:
    calls: list[tuple[str, str, list[str]]] = []

    def fake_native(operation: str, key: str, flags: list[str]) -> dict:
        calls.append((operation, key, flags))
        return _claim_payload(key)

    monkeypatch.setattr(core, "_native_claim", fake_native)

    claim = core.acquire_claim(
        "node:x-test",
        "holder-1",
        ttl_ms=60_000,
        reason="test",
        root=tmp_path,
    )

    assert claim.key == "node:x-test"
    assert calls == [
        (
            "acquire",
            "node:x-test",
            [
                "--holder",
                "holder-1",
                "--pid",
                str(os.getpid()),
                "--ttl-ms",
                "60000",
                "--reason",
                "test",
                "--root",
                str(tmp_path),
            ],
        )
    ]


def test_claim_status_uses_native_json_door(monkeypatch, tmp_path: Path) -> None:
    calls: list[tuple[str, str, list[str]]] = []

    def fake_native(operation: str, key: str, flags: list[str]) -> dict:
        calls.append((operation, key, flags))
        return {"key": key, "state": "free"}

    monkeypatch.setattr(core, "_native_claim", fake_native)

    status = core.claim_status("node:x-test", root=tmp_path)

    assert status == {"key": "node:x-test", "state": "free"}
    assert calls == [("status", "node:x-test", ["--root", str(tmp_path)])]
