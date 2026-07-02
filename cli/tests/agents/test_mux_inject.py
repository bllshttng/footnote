"""Mail live-inject dispatch on the registry mux ref (4a-G2/G3, task 4.9/4.10).

A mux-hosted row routes through `fno mux pane send` with the writer claim held
around the text-then-CR burst; a dead pane fails closed so the caller demotes
to the durable bus. The mux subprocess is faked; the real socket path is the
agent_edge e2e.
"""
from __future__ import annotations

import subprocess
from pathlib import Path

import pytest

from fno.paths_testing import use_tmpdir


def _mux_entry(name: str = "muxed", provider: str = "claude"):
    from fno.agents.registry import AgentEntry

    return AgentEntry(
        name=name,
        provider=provider,
        cwd="/w",
        log_path="",
        status="live",
        mux={"session": "work", "pane_id": 7},
    )


class FakeMux:
    """Record `fno mux pane <verb> ...` calls; script per-verb exit codes."""

    def __init__(self, fail_verbs: set[str] | None = None) -> None:
        self.calls: list[tuple[list[str], str | None]] = []
        self.fail_verbs = fail_verbs or set()

    def __call__(self, argv, input=None, **kwargs):
        verb = argv[3]
        self.calls.append((list(argv), input))
        rc = 1 if verb in self.fail_verbs else 0
        return subprocess.CompletedProcess(argv, rc, "", "boom" if rc else "")


@pytest.fixture(autouse=True)
def _fast(monkeypatch):
    from fno.agents import dispatch as dispatch_mod

    monkeypatch.setattr(dispatch_mod.time, "sleep", lambda _s: None)


def _patch_mux(monkeypatch, fake: FakeMux) -> None:
    from fno.agents import dispatch as dispatch_mod

    monkeypatch.setattr(dispatch_mod.subprocess, "run", fake)


def test_inject_mux_row_claims_sends_and_releases(monkeypatch) -> None:
    from fno.agents.dispatch import _mux_pane_send

    fake = FakeMux()
    _patch_mux(monkeypatch, fake)
    assert _mux_pane_send(_mux_entry(), "<fno_mail>hi</fno_mail>") is True

    verbs = [c[0][3] for c in fake.calls]
    assert verbs == ["claim", "send", "send", "release"]
    for argv, _ in fake.calls:
        assert argv[argv.index("--session") + 1] == "work"
        assert argv[4] == "7"
    # Envelope bytes ride --stdin verbatim; the CR submit is its own send.
    send_text, cr = fake.calls[1], fake.calls[2]
    assert "--stdin" in send_text[0]
    assert send_text[1] == "<fno_mail>hi</fno_mail>"
    assert cr[0][cr[0].index("--text") + 1] == "\r"


def test_inject_dead_pane_fails_closed_and_still_releases(monkeypatch) -> None:
    from fno.agents.dispatch import _mux_pane_send

    fake = FakeMux(fail_verbs={"send"})
    _patch_mux(monkeypatch, fake)
    assert _mux_pane_send(_mux_entry(), "hi") is False
    verbs = [c[0][3] for c in fake.calls]
    assert verbs == ["claim", "send", "release"], "no CR after a failed send"


def test_inject_claim_refusal_is_fail_open(monkeypatch) -> None:
    # A pane spawned without --claim refuses the acquire; the send proceeds
    # and no release is issued.
    from fno.agents.dispatch import _mux_pane_send

    fake = FakeMux(fail_verbs={"claim"})
    _patch_mux(monkeypatch, fake)
    assert _mux_pane_send(_mux_entry(), "hi") is True
    verbs = [c[0][3] for c in fake.calls]
    assert verbs == ["claim", "send", "send"]


def test_deliver_live_dispatches_on_mux_ref_before_legacy_lanes(
    tmp_path: Path, monkeypatch
) -> None:
    # Dual-run: a mux row (any provider) never touches the daemon RPC or the
    # control.sock lanes; a worker/bg row still does (AC3 + AC5-FR).
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents import dispatch as dispatch_mod

    seen = []
    monkeypatch.setattr(
        dispatch_mod, "_mux_pane_send", lambda entry, text: seen.append(entry.name) or True
    )
    monkeypatch.setattr(
        dispatch_mod,
        "_daemon_rpc",
        lambda *a, **k: pytest.fail("mux row must not hit the daemon RPC"),
    )
    assert dispatch_mod._deliver_live(_mux_entry(provider="codex"), "hi", "fno")
    assert seen == ["muxed"]

    # A legacy codex worker row (no mux ref) still routes to the daemon.
    calls = []
    monkeypatch.setattr(
        dispatch_mod, "_daemon_rpc", lambda *a, **k: calls.append(a) or {"delivered": True}
    )
    from fno.agents.registry import AgentEntry

    worker = AgentEntry(
        name="wk", provider="codex", cwd="/w", log_path="", short_id="wk-1"
    )
    assert dispatch_mod._deliver_live(worker, "hi", "fno") is True
    assert calls, "worker row keeps the legacy daemon lane during dual-run"


# ---------------------------------------------------------------------------
# `fno agents ask` follow-up on a mux row (routing fix)
# ---------------------------------------------------------------------------
# Before the fix, dispatch_ask routed a mux row to the provider follow-up
# path, which keys on claude_short_id / codex_session_id / gemini_session_id
# a mux row lacks, and raised exit 12. It must ride PaneSend instead.


def _seed(entry) -> None:
    from fno.agents.registry import write_registry

    write_registry([entry])


def test_ask_mux_row_rides_pane_send(tmp_path: Path, monkeypatch) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    _seed(_mux_entry())  # claude provider, mux ref, NO claude_short_id
    fake = FakeMux()
    _patch_mux(monkeypatch, fake)

    from fno.agents.dispatch import dispatch_ask

    result = dispatch_ask("muxed", "ping", provider=None, cwd=Path("/w"))

    assert result.kind == "followup"
    assert result.reply == ""  # fire-and-forget: no captured reply
    assert result.short_id == "work:7"
    verbs = [c[0][3] for c in fake.calls]
    assert verbs == ["claim", "send", "send", "release"]
    assert fake.calls[1][1] == "ping"  # the message rode --stdin verbatim


def test_ask_mux_dead_pane_raises_transport_error(
    tmp_path: Path, monkeypatch
) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    _seed(_mux_entry())
    fake = FakeMux(fail_verbs={"send"})
    _patch_mux(monkeypatch, fake)

    from fno.agents.dispatch import DispatchAskError, dispatch_ask

    with pytest.raises(DispatchAskError) as exc:
        dispatch_ask("muxed", "ping", provider=None, cwd=Path("/w"))
    assert exc.value.exit_code == 1  # transport failure, not the old exit 12


def test_ask_mux_codex_row_also_rides_pane_send(
    tmp_path: Path, monkeypatch
) -> None:
    """Mux routing is provider-independent: a mux codex row rides PaneSend too."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed(_mux_entry(name="cmux", provider="codex"))
    fake = FakeMux()
    _patch_mux(monkeypatch, fake)

    from fno.agents.dispatch import dispatch_ask

    result = dispatch_ask("cmux", "ping", provider=None, cwd=Path("/w"))
    assert result.kind == "followup"
    assert [c[0][3] for c in fake.calls] == ["claim", "send", "send", "release"]
