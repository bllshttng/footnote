"""Tests for fno.agents.dispatch and providers.base — TDD Red phase.

Acceptance criteria from Task 1.2:

- ProviderResult dataclass (exit_code, stdout, stderr, duration_ms, session_id_out)
- Provider availability detection — `which {claude,codex,gemini}` returns booleans
- dispatch.select_provider() honors registry provider field; rejects mismatch on follow-up
"""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.paths_testing import use_tmpdir


# ---------------------------------------------------------------------------
# ProviderResult dataclass
# ---------------------------------------------------------------------------


def test_provider_result_fields() -> None:
    """ProviderResult exposes exit_code, stdout, stderr, duration_ms, session_id_out."""
    from fno.agents.providers.base import ProviderResult

    result = ProviderResult(
        exit_code=0,
        stdout="hello",
        stderr="",
        duration_ms=42,
        session_id_out="abc12345",
    )
    assert result.exit_code == 0
    assert result.stdout == "hello"
    assert result.stderr == ""
    assert result.duration_ms == 42
    assert result.session_id_out == "abc12345"


def test_provider_result_session_id_optional() -> None:
    """session_id_out defaults to None for providers that don't return one."""
    from fno.agents.providers.base import ProviderResult

    result = ProviderResult(
        exit_code=1,
        stdout="",
        stderr="boom",
        duration_ms=100,
    )
    assert result.session_id_out is None


# ---------------------------------------------------------------------------
# Provider availability detection
# ---------------------------------------------------------------------------


def test_is_provider_available_returns_bool(monkeypatch) -> None:
    """is_provider_available(name) returns True/False based on `which` lookup."""
    from fno.agents.dispatch import is_provider_available

    # Force a known-missing binary to verify False path
    monkeypatch.setenv("PATH", "/nonexistent/path")
    assert is_provider_available("claude") is False
    assert is_provider_available("codex") is False
    assert is_provider_available("gemini") is False


def test_is_provider_available_finds_real_binary(tmp_path: Path, monkeypatch) -> None:
    """is_provider_available returns True when the binary is on PATH."""
    from fno.agents.dispatch import is_provider_available

    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    fake = bin_dir / "claude"
    fake.write_text("#!/bin/sh\nexit 0\n")
    fake.chmod(0o755)
    monkeypatch.setenv("PATH", str(bin_dir))

    assert is_provider_available("claude") is True


def test_is_provider_available_rejects_unknown_name() -> None:
    """is_provider_available rejects names outside the known set."""
    from fno.agents.dispatch import is_provider_available

    with pytest.raises(ValueError, match="unknown provider"):
        is_provider_available("not-a-real-provider")


def test_available_providers_returns_dict(monkeypatch) -> None:
    """available_providers() returns {name: bool} for all known providers."""
    from fno.agents.dispatch import available_providers

    monkeypatch.setenv("PATH", "/nonexistent")
    result = available_providers()
    assert set(result.keys()) == {"claude", "codex", "gemini"}
    assert all(v is False for v in result.values())


# ---------------------------------------------------------------------------
# dispatch.select_provider — registry mismatch rejection
# ---------------------------------------------------------------------------


def test_select_provider_returns_requested_for_new_agent(tmp_path: Path, monkeypatch) -> None:
    """select_provider returns the requested provider for a name not yet in the registry."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.dispatch import select_provider

    chosen = select_provider(name="brand-new", requested_provider="claude")
    assert chosen == "claude"


def test_select_provider_returns_registered_when_no_request(tmp_path: Path, monkeypatch) -> None:
    """When the agent exists and no provider is requested, select_provider returns the recorded one."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.dispatch import select_provider
    from fno.agents.registry import AgentEntry, write_registry

    write_registry(
        [AgentEntry(name="existing", harness="codex", cwd="/tmp", log_path="/tmp/x.log")]
    )

    chosen = select_provider(name="existing", requested_provider=None)
    assert chosen == "codex"


def test_select_provider_rejects_mismatch_on_follow_up(tmp_path: Path, monkeypatch) -> None:
    """When requested_provider disagrees with the recorded provider, raise ProviderMismatchError."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.dispatch import ProviderMismatchError, select_provider
    from fno.agents.registry import AgentEntry, write_registry

    write_registry(
        [AgentEntry(name="locked", harness="claude", cwd="/tmp", log_path="/tmp/y.log")]
    )

    with pytest.raises(ProviderMismatchError) as exc_info:
        select_provider(name="locked", requested_provider="codex")
    msg = str(exc_info.value)
    assert "locked" in msg
    assert "claude" in msg  # recorded
    assert "codex" in msg  # requested


def test_select_provider_accepts_matching_request(tmp_path: Path, monkeypatch) -> None:
    """Passing the same provider as recorded is fine — no mismatch error."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.dispatch import select_provider
    from fno.agents.registry import AgentEntry, write_registry

    write_registry(
        [AgentEntry(name="match", harness="gemini", cwd="/tmp", log_path="/tmp/m.log")]
    )

    chosen = select_provider(name="match", requested_provider="gemini")
    assert chosen == "gemini"


def test_select_provider_requires_provider_for_new_agent(tmp_path: Path, monkeypatch) -> None:
    """A first-time ask with no requested_provider has nothing to select — raise ValueError."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.dispatch import select_provider

    with pytest.raises(ValueError, match="provider is required for new agent"):
        select_provider(name="brand-new", requested_provider=None)


def test_select_provider_rejects_unknown_provider(tmp_path: Path, monkeypatch) -> None:
    """select_provider rejects an unsupported provider name."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.dispatch import select_provider

    with pytest.raises(ValueError, match="unknown provider"):
        select_provider(name="x", requested_provider="not-real")
