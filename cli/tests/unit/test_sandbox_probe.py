"""The sandbox probe bridge (x-6863 task 3.2).

The probe itself is Rust (crates/fno-agents/src/sandbox_probe.rs); these tests
pin the bridge contract: the verdict/blocked/note triple crosses the verb
boundary, an unavailable owner answers `unknown` with the reason (never a
guessed `reachable`), and the exit-85 contract stays Python-side. No test
runs a live `codex sandbox` - the binary's own unit tests cover the argv and
the canary semantics."""
from __future__ import annotations

from pathlib import Path

from fno.agents.sandbox_probe import SandboxProbe, probe_codex_sandbox


def test_bridge_replays_the_rust_answer(tmp_path):
    from unittest import mock

    fake = {
        "verdict": "reachable",
        "blocked": [["gh", "exit 1"]],
        "note": "",
        "posture": "workspace-write:on-request",
    }
    with mock.patch(
        "fno.rust_binary.verb_call", return_value=fake
    ) as call:
        probe = probe_codex_sandbox(tmp_path, mode="workspace-write:on-request")
    assert probe.verdict == "reachable"
    assert probe.blocked == [("gh", "exit 1")]
    assert call.call_args.args[0] == "sandbox-probe"
    assert call.call_args.args[1] == {
        "cwd": str(tmp_path),
        "mode": "workspace-write:on-request",
    }


def test_bridge_answers_unknown_when_the_owner_is_unavailable(tmp_path):
    from unittest import mock

    with mock.patch(
        "fno.rust_binary.verb_call",
        side_effect=_unavailable,
    ):
        probe = probe_codex_sandbox(tmp_path)
    assert probe.verdict == "unknown"
    assert "unavailable" in probe.note


def _unavailable(*_args, **_kwargs):
    from fno.rust_binary import VerbUnavailable

    raise VerbUnavailable("binary missing")


def test_exit_contract_constant_is_untouched():
    from fno.agents.sandbox_probe import EXIT_SANDBOX_UNREACHABLE

    assert EXIT_SANDBOX_UNREACHABLE == 85
    assert isinstance(SandboxProbe("unknown"), SandboxProbe)