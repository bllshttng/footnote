from __future__ import annotations

import pytest


def test_claim_clock_lives_with_claim_types():
    from fno.claims.types import now_ms

    before = now_ms()
    after = now_ms()

    assert after >= before


def test_missing_agents_binary_refuses_without_a_python_fallback(monkeypatch):
    from fno.claims import verdict

    monkeypatch.setattr(verdict, "resolve_binary", lambda: None)

    with pytest.raises(verdict.ClaimVerdictUnavailable, match="FNO_AGENTS_BIN") as exc:
        verdict.claim_verdicts(["node:missing"])

    assert "reinstall" in str(exc.value).lower()


def test_many_keys_use_one_native_batch_subprocess(tmp_path, monkeypatch):
    from fno.claims import verdict

    count = tmp_path / "invocations"
    binary = tmp_path / "fno-agents"
    binary.write_text(
        "#!/bin/sh\n"
        f"printf '1' > '{count}'\n"
        "printf '%s' '{\"claims\":["
        "{\"key\":\"node:a\",\"state\":\"live\"},"
        "{\"key\":\"node:b\",\"state\":\"suspect\"}]}'\n"
    )
    binary.chmod(0o755)
    monkeypatch.setattr(verdict, "resolve_binary", lambda: binary)

    rows = verdict.claim_verdicts(["node:a", "node:b"])

    assert count.read_text() == "1"
    assert rows == {
        "node:a": {"key": "node:a", "state": "live"},
        "node:b": {"key": "node:b", "state": "suspect"},
    }
