"""Task 1.2: spawn verb with --once / -o ephemeral one-shot lifecycle.

Acceptance criteria (operator-locked):

  AC2-HP: codex --once creates+exchanges, stdout=reply, exit 0, teardown receipt on
          stderr, registry has NO row afterward.
  AC2-ERR: provider create fails -> stderr has error, nonzero exit, registry empty.
  AC2-UI: stderr receipt identifies peer (name, provider, session_or_short_id).
  AC2-EDGE: pre-seeded name -> collision refuse exit 2, row untouched.
  AC2-FR: teardown fails after successful exchange -> stderr warning names peer +
          fno agents rm hint, exit 0, row still present.
  claude plain spawn: JSON receipt on stdout exact-match, registry row present with
          provider=claude.
  claude --once refusal exit 2.
  codex plain-spawn (no --once) refusal exit 13 (Python fallback, PTY daemon needed).
  CLI wiring: fno agents spawn registered; RUST_ONLY_VERB_HELP no longer lists spawn;
          help-parity test passes (implicitly via test_rust_only_verb_help_covers_unregistered_verbs).
"""
from __future__ import annotations

import json
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest
from typer.testing import CliRunner

from fno.paths_testing import use_tmpdir
from fno.agents import events as events_mod
from fno.agents.providers import codex as codex_mod
from fno.agents.providers.codex import (
    CodexInvocationError,
    CodexResult,
    CodexTimeoutError,
    NoSessionIdError,
)
from fno.agents.registry import (
    AgentEntry,
    load_registry,
    write_registry,
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _make_runner() -> CliRunner:
    return CliRunner()


def _read_events(tmp_path: Path) -> list[dict]:
    from fno import paths
    events_log = paths.state_dir() / "events.jsonl"
    if not events_log.exists():
        return []
    return [
        json.loads(line)
        for line in events_log.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]


def _write_existing_entry(name: str, provider: str, session_id: str) -> None:
    """Seed the registry with one entry for collision tests."""
    write_registry([
        AgentEntry(
            name=name,
            harness=provider,
            cwd="/tmp",
            log_path="/tmp/a.log",
            harness_session_id=session_id if provider == "codex" else None,
            short_id=session_id if provider == "claude" else "",
        )
    ])


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture
def workdir(tmp_path, monkeypatch):
    """Isolated fno home with codex marked available on PATH."""
    use_tmpdir(monkeypatch, tmp_path)
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    (bin_dir / "codex").write_text("#!/bin/sh\necho fake\n", encoding="utf-8")
    (bin_dir / "codex").chmod(0o755)
    monkeypatch.setenv("PATH", str(bin_dir))
    return tmp_path


@pytest.fixture
def workdir_claude(tmp_path, monkeypatch):
    """Isolated fno home with claude marked available on PATH."""
    from tests.agents._fake_claude import install_fake_claude
    use_tmpdir(monkeypatch, tmp_path)
    bin_dir = tmp_path / "bin"
    install_fake_claude(bin_dir)
    monkeypatch.setenv("PATH", str(bin_dir))
    return tmp_path


@pytest.fixture
def fake_codex_create_once(monkeypatch):
    """Replace codex_mod.create with a mock returning a successful CodexResult."""
    mock = MagicMock(return_value=CodexResult(
        exit_code=0,
        session_id="codex-once-sid",
        last_msg="hello from codex",
        duration_ms=42,
    ))
    monkeypatch.setattr(codex_mod, "create", mock)
    return mock


# ---------------------------------------------------------------------------
# AC2-HP: codex --once happy path
# ---------------------------------------------------------------------------


def test_spawn_once_codex_happy_path(workdir, fake_codex_create_once, monkeypatch) -> None:
    """AC2-HP: codex --once creates+exchanges, reply on stdout, teardown on stderr,
    no registry row afterward, exit 0."""
    from fno.agents.cli import agents_app

    runner = _make_runner()
    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "tmp1", "-H", "codex", "--once", "summarize X"],
        catch_exceptions=False,
    )

    assert result.exit_code == 0, f"expected exit 0, got {result.exit_code}\noutput: {result.output}"
    # stdout+stderr combined is result.output in Typer CliRunner
    assert "hello from codex" in result.output
    # Teardown receipt in output
    assert "tmp1" in result.output
    assert "torn down" in result.output or "teardown" in result.output.lower()
    # Registry must have NO row for tmp1 after teardown
    entries = load_registry()
    assert not any(e.name == "tmp1" for e in entries), (
        f"Expected no tmp1 row after --once teardown, got: {entries}"
    )


# ---------------------------------------------------------------------------
# AC2-ERR: provider create fails -> no registry entry, nonzero exit
# ---------------------------------------------------------------------------


def test_spawn_once_create_failure_no_registry_entry(workdir, monkeypatch) -> None:
    """AC2-ERR: codex create fails -> stderr has error, nonzero exit, no registry row."""
    monkeypatch.setattr(
        codex_mod, "create",
        MagicMock(side_effect=CodexInvocationError(1))
    )
    from fno.agents.cli import agents_app

    runner = _make_runner()
    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "tmp2", "-H", "codex", "--once", "hello"],
    )

    assert result.exit_code != 0, (
        f"expected nonzero exit on create failure, got {result.exit_code}\noutput: {result.output}"
    )
    entries = load_registry()
    assert not any(e.name == "tmp2" for e in entries), (
        "No registry row should exist after failed create"
    )


# ---------------------------------------------------------------------------
# AC2-UI: stderr receipt format (name, provider, session_or_short_id)
# ---------------------------------------------------------------------------


def test_spawn_once_receipt_format(workdir, fake_codex_create_once) -> None:
    """AC2-UI: teardown receipt on stderr identifies peer: name + provider/id."""
    from fno.agents.cli import agents_app

    runner = _make_runner()
    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "myagent", "-H", "codex", "--once", "do something"],
        catch_exceptions=False,
    )

    assert result.exit_code == 0
    # Receipt (in combined output) must contain name and provider
    assert "myagent" in result.output
    assert "codex" in result.output


# ---------------------------------------------------------------------------
# AC2-EDGE: name collision refuses, existing row untouched
# ---------------------------------------------------------------------------


def test_spawn_collision_refuses(workdir) -> None:
    """AC2-EDGE: pre-seeded registry entry -> spawn refuses exit 2, row untouched."""
    _write_existing_entry("existing-agent", "codex", "oldses-123")

    from fno.agents.cli import agents_app

    runner = _make_runner()
    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "existing-agent", "-H", "codex", "--once", "hello"],
    )

    assert result.exit_code == 2, (
        f"expected exit 2 for name collision, got {result.exit_code}\n"
        f"output: {result.output}"
    )
    # Message must mention the name and hint
    assert "existing-agent" in result.output
    assert "rm" in result.output
    # Row must remain intact
    entries = load_registry()
    existing = next((e for e in entries if e.name == "existing-agent"), None)
    assert existing is not None, "existing row must not be deleted on collision"
    assert existing.harness_session_id == "oldses-123"


# ---------------------------------------------------------------------------
# AC2-FR: teardown failure after successful exchange
# ---------------------------------------------------------------------------


def test_spawn_once_teardown_failure(workdir, fake_codex_create_once, monkeypatch) -> None:
    """AC2-FR: teardown fails after successful exchange -> stderr warning, exit 0,
    row still present."""
    from fno.agents.cli import agents_app
    from fno.agents import dispatch as dispatch_mod

    # Monkeypatch update_registry to fail during teardown but succeed during create.
    original_update = dispatch_mod.update_registry
    call_count = [0]

    def _patched_update_registry(updater):
        call_count[0] += 1
        if call_count[0] > 1:
            # Second call is the teardown removal - make it fail
            from fno.agents.registry import RegistryVersionError
            raise OSError("simulated teardown failure")
        return original_update(updater)

    monkeypatch.setattr(dispatch_mod, "update_registry", _patched_update_registry)

    runner = _make_runner()
    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "teardown-victim", "-H", "codex", "--once", "hello"],
        catch_exceptions=False,
    )

    # Exchange succeeded, so exit 0 even though teardown failed
    assert result.exit_code == 0, (
        f"expected exit 0 (exchange succeeded), got {result.exit_code}\n"
        f"output: {result.output}"
    )
    # Output must warn about the leaked peer and hint at rm
    assert "teardown-victim" in result.output
    assert "rm" in result.output
    # Row must still be present (teardown didn't clean it)
    entries = load_registry()
    assert any(e.name == "teardown-victim" for e in entries), (
        "Row must remain visible after failed teardown (AC2-FR)"
    )


# ---------------------------------------------------------------------------
# claude plain spawn: compact JSON receipt, registry row present
# ---------------------------------------------------------------------------


def test_spawn_claude_plain(workdir_claude) -> None:
    """claude bg-substrate spawn: compact JSON receipt on stdout (4a-G2: the
    plain/pane default is mux-hosted; the bg thread lane keeps this shape)."""
    from fno.agents.cli import agents_app

    runner = _make_runner()
    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "myagent-c", "-H", "claude", "hello", "--substrate", "bg"],
        catch_exceptions=False,
    )

    assert result.exit_code == 0, (
        f"expected exit 0, got {result.exit_code}\noutput: {result.output}"
    )
    # output is stdout+stderr combined in Typer CliRunner; the JSON receipt
    # is the FIRST line (on stdout), before any stderr teardown notes.
    first_line = result.output.split("\n")[0].strip()
    receipt = json.loads(first_line)
    assert receipt["name"] == "myagent-c"
    assert receipt["provider"] == "claude"
    assert receipt["status"] == "live"
    assert "short_id" in receipt
    # jq .short_id must work (i.e. it's a plain string value)
    assert isinstance(receipt["short_id"], str)
    assert len(receipt["short_id"]) == 8

    # Verify the exact format: hand-rolled, keys in order name/short_id/provider/status
    assert first_line == (
        f'{{"name": "{receipt["name"]}", "short_id": "{receipt["short_id"]}", '
        f'"provider": "claude", "status": "live"}}'
    )

    # Registry row must be present
    entries = load_registry()
    entry = next((e for e in entries if e.name == "myagent-c"), None)
    assert entry is not None, "registry row must exist after claude spawn"
    assert entry.harness == "claude"
    assert entry.short_id == receipt["short_id"]


def test_spawn_claude_receipt_surfaces_moved_cwd(workdir_claude, monkeypatch) -> None:
    """x-85fe: when the default moves the worker off the caller (canonical !=
    caller), the bg receipt appends the effective cwd LAST, and the stderr
    redirect note fires (AC1-HP / AC1-UI). The unmoved receipt stays byte-
    identical (proven by test_spawn_claude_plain)."""
    from fno.agents.cli import agents_app

    canon = workdir_claude / "canon"
    canon.mkdir()
    monkeypatch.setenv("FNO_REPO_ROOT", str(canon))

    runner = _make_runner()
    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "moved-c", "-H", "claude", "hello", "--substrate", "bg"],
        catch_exceptions=False,
    )
    assert result.exit_code == 0, result.output
    first_line = result.output.split("\n")[0].strip()
    receipt = json.loads(first_line)
    assert receipt["cwd"] == str(canon.resolve())
    # cwd is the LAST key (byte-parity contract with Rust claude_ask).
    assert first_line.rstrip("}").rstrip().endswith(f'"cwd": "{canon.resolve()}"')
    assert "dispatching from canonical main" in result.output


def test_spawn_claude_receipt_cwd_json_encoded(workdir_claude, monkeypatch) -> None:
    """x-85fe (codex #4): a canonical path with a backslash must stay valid JSON
    in the receipt. A bare `"`-escape would emit `\\n`-style sequences that
    json.loads mis-decodes or rejects; json.dumps keeps it parseable and matches
    the Rust json_string_ascii twin."""
    from fno.agents.cli import agents_app

    canon = workdir_claude / "ca\\non"  # a dir literally named ca\non
    canon.mkdir()
    monkeypatch.setenv("FNO_REPO_ROOT", str(canon))

    runner = _make_runner()
    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "bs-c", "-H", "claude", "hello", "--substrate", "bg"],
        catch_exceptions=False,
    )
    assert result.exit_code == 0, result.output
    first_line = result.output.split("\n")[0].strip()
    receipt = json.loads(first_line)  # must not raise
    assert receipt["cwd"] == str(canon.resolve())


# ---------------------------------------------------------------------------
# claude --once: refused, exit 2
# ---------------------------------------------------------------------------


def test_spawn_claude_once_refused(workdir_claude) -> None:
    """claude --once -> exit 2 with explanation that claude peers are persistent."""
    from fno.agents.cli import agents_app

    runner = _make_runner()
    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "cagent", "-H", "claude", "--once", "hello"],
    )

    assert result.exit_code == 2, (
        f"expected exit 2 for claude --once refusal, got {result.exit_code}\n"
        f"output: {result.output}"
    )
    assert "claude" in result.output.lower()
    # Must explain the restriction
    assert "--once" in result.output or "persistent" in result.output or "not supported" in result.output


# ---------------------------------------------------------------------------
# codex plain spawn (no --once): refused exit 13 in Python fallback
# ---------------------------------------------------------------------------


def test_spawn_codex_plain_no_once_refused(workdir, monkeypatch) -> None:
    """codex bg-substrate spawn (no --once) in Python fallback -> exit 13 (the
    daemon-worker lane; the pane default routes to the mux instead, 4a-G2)."""
    from fno.agents.cli import agents_app

    runner = _make_runner()
    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "ptagent", "-H", "codex", "hello", "--substrate", "bg"],
    )

    assert result.exit_code == 13, (
        f"expected exit 13 for plain codex spawn in Python fallback, got {result.exit_code}\n"
        f"output: {result.output}"
    )
    assert "--once" in result.output or "daemon" in result.output or "Rust" in result.output


def test_spawn_unknown_provider_exits_2(workdir) -> None:
    """Unknown --provider -> clean exit 2 (not a ValueError traceback).

    _check_known_provider raises ValueError; dispatch_spawn must wrap it as
    DispatchAskError(exit_code=2) because cmd_spawn only catches the latter
    (Task 1.3 parity hardening - the Rust client prints the same message).
    """
    from fno.agents.cli import agents_app

    runner = _make_runner()
    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "fooagent", "--harness", "foo", "hello"],
    )

    assert result.exit_code == 2, (
        f"expected exit 2 for unknown provider, got {result.exit_code}\n"
        f"output: {result.output}"
    )
    assert "unknown provider 'foo'" in result.output
    # Default substrate is `pane`, so the READABLE_PROVIDERS pane gate rejects
    # (x-8f7f): the message names the pane-hostable set (agy included), not the
    # narrower Python-dispatchable KNOWN_PROVIDERS set.
    assert (
        "pane-hostable providers: claude, codex, gemini, agy, opencode"
        in result.output
    )


# ---------------------------------------------------------------------------
# CLI wiring: spawn verb is registered
# ---------------------------------------------------------------------------


def test_spawn_verb_registered() -> None:
    """fno agents spawn is a registered command (Python-implemented)."""
    from fno.agents.cli import agents_app

    registered = {cmd.name for cmd in agents_app.registered_commands}
    assert "spawn" in registered, (
        f"'spawn' must be registered as a Python command. Got: {sorted(registered)}"
    )


# ---------------------------------------------------------------------------
# RUST_ONLY_VERB_HELP no longer lists spawn
# ---------------------------------------------------------------------------


def test_spawn_not_in_rust_only_verb_help() -> None:
    """spawn is Python-registered, so it must NOT appear in RUST_ONLY_VERB_HELP."""
    from fno.agents import rust_runtime as rr

    assert "spawn" not in rr.RUST_ONLY_VERB_HELP, (
        "spawn has a Python implementation; it must not be in RUST_ONLY_VERB_HELP"
    )
