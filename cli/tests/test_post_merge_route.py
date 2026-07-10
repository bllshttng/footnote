"""Warm-session resolver + inject mapping for the post-merge ritual."""
from __future__ import annotations

from dataclasses import dataclass

import pytest

from fno.post_merge_route import inject_pr_merged, resolve_warm_session


@dataclass
class _Sess:
    session_id: str


def _patch_live(monkeypatch, sessions):
    import fno.agents.discover as discover

    monkeypatch.setattr(
        discover, "discover_live_sessions", lambda **_kw: sessions
    )


class TestResolveWarmSession:
    def test_live_source_session_resolves(self, monkeypatch):
        _patch_live(monkeypatch, [_Sess("aaaa-bbbb"), _Sess("cccc-dddd")])
        monkeypatch.delenv("CLAUDE_SESSION_ID", raising=False)
        assert resolve_warm_session("cccc-dddd") == "cccc-dddd"

    def test_dead_or_unknown_session_is_none(self, monkeypatch):
        _patch_live(monkeypatch, [_Sess("aaaa-bbbb")])
        monkeypatch.delenv("CLAUDE_SESSION_ID", raising=False)
        assert resolve_warm_session("gone-gone") is None

    @pytest.mark.parametrize("empty", [None, "", "   "])
    def test_missing_id_is_none(self, monkeypatch, empty):
        _patch_live(monkeypatch, [_Sess("aaaa-bbbb")])
        assert resolve_warm_session(empty) is None

    @pytest.mark.parametrize(
        "env_var",
        [
            "CODEX_THREAD_ID",
            "CLAUDE_CODE_SESSION_ID",
            "CODEX_SESSION_ID",
            "GEMINI_SESSION_ID",
            "CLAUDE_SESSION_ID",
        ],
    )
    def test_never_self_injects(self, monkeypatch, env_var):
        """The self-guard must fire against whichever ambient env var carries the
        running session id. source_session_id is stamped from
        CLAUDE_CODE_SESSION_ID, so a guard checking only CLAUDE_SESSION_ID would
        miss it and inject the ritual into the running session."""
        for v in (
            "CODEX_THREAD_ID",
            "CLAUDE_CODE_SESSION_ID",
            "CODEX_SESSION_ID",
            "GEMINI_SESSION_ID",
            "CLAUDE_SESSION_ID",
        ):
            monkeypatch.delenv(v, raising=False)
        _patch_live(monkeypatch, [_Sess("me-me-me")])
        monkeypatch.setenv(env_var, "me-me-me")
        assert resolve_warm_session("me-me-me") is None

    def test_resolver_error_degrades_to_none(self, monkeypatch):
        import fno.agents.discover as discover

        def _boom(**_kw):
            raise RuntimeError("registry unreadable")

        monkeypatch.setattr(discover, "discover_live_sessions", _boom)
        monkeypatch.delenv("CLAUDE_SESSION_ID", raising=False)
        assert resolve_warm_session("aaaa-bbbb") is None


class TestInjectPrMerged:
    def _patch_submit(self, monkeypatch, outcome, capture=None):
        import fno.relay.roundtrip as rt

        def _fake(session_id, framed):
            if capture is not None:
                capture.append((session_id, framed))
            return outcome

        monkeypatch.setattr(rt, "submit_via_control_reply", _fake)

    def test_confirmed_is_delivered(self, monkeypatch):
        sent: list = []
        self._patch_submit(monkeypatch, "confirmed", sent)
        delivered, reason = inject_pr_merged("aaaa-bbbb", 123)
        assert delivered is True
        assert reason == "delivered"
        assert sent == [("aaaa-bbbb", "/fno:pr merged 123 autonomous")]

    def test_unconfirmed_maps_to_queue_timeout(self, monkeypatch):
        self._patch_submit(monkeypatch, "unconfirmed")
        assert inject_pr_merged("aaaa-bbbb", 7) == (False, "queue-timeout")

    def test_not_sent_maps_to_not_live(self, monkeypatch):
        self._patch_submit(monkeypatch, "not_sent")
        assert inject_pr_merged("aaaa-bbbb", 7) == (False, "not-live")

    def test_submit_exception_is_contained(self, monkeypatch):
        import fno.relay.roundtrip as rt

        def _boom(session_id, framed):
            raise OSError("socket vanished")

        monkeypatch.setattr(rt, "submit_via_control_reply", _boom)
        delivered, reason = inject_pr_merged("aaaa-bbbb", 7)
        assert delivered is False
        assert reason.startswith("inject-error")


class _FakeEntry:
    def __init__(self, provider, codex_session_id, status="live"):
        self.provider = provider
        self.codex_session_id = codex_session_id
        self.status = status


def _patch_registry(monkeypatch, entries):
    import fno.agents.registry as reg

    monkeypatch.setattr(reg, "load_registry", lambda *a, **k: entries)


class TestCodexWarmRoute:
    """x-c4dd: codex-shipped nodes warm-route to their live registered panel via
    the shared _deliver_live vehicle, injecting the RAW command (mail=None)."""

    def test_resolve_codex_live_registered_panel(self, monkeypatch):
        monkeypatch.delenv("CLAUDE_SESSION_ID", raising=False)
        _patch_registry(monkeypatch, [_FakeEntry("codex", "cx-123", "live")])
        assert resolve_warm_session("cx-123", "codex") == "cx-123"

    def test_resolve_codex_no_panel_is_none(self, monkeypatch):
        _patch_registry(monkeypatch, [])
        assert resolve_warm_session("cx-123", "codex") is None

    def test_resolve_codex_exited_panel_is_none(self, monkeypatch):
        _patch_registry(monkeypatch, [_FakeEntry("codex", "cx-123", "exited")])
        assert resolve_warm_session("cx-123", "codex") is None

    def test_resolve_gemini_always_cold(self, monkeypatch):
        # No live-inject vehicle yet (US9): gemini cold-paths regardless.
        _patch_registry(monkeypatch, [_FakeEntry("gemini", "gm-1", "live")])
        assert resolve_warm_session("gm-1", "gemini") is None

    def test_inject_codex_delivers_raw_via_deliver_live(self, monkeypatch):
        entry = _FakeEntry("codex", "cx-9", "live")
        _patch_registry(monkeypatch, [entry])
        sent = {}
        import fno.agents.dispatch as dispatch

        def _fake_deliver(e, body, from_name="fno", mail=None):
            sent["args"] = (e, body, mail)
            return True

        monkeypatch.setattr(dispatch, "_deliver_live", _fake_deliver)
        delivered, reason = inject_pr_merged("cx-9", 42, "codex")
        assert (delivered, reason) == (True, "delivered")
        # RAW command, no <fno_mail> envelope: mail is None so it lands verbatim.
        assert sent["args"][0] is entry
        assert sent["args"][1] == "/fno:pr merged 42 autonomous"
        assert sent["args"][2] is None

    def test_inject_codex_no_panel_is_not_live(self, monkeypatch):
        _patch_registry(monkeypatch, [])
        assert inject_pr_merged("cx-9", 42, "codex") == (False, "not-live")

    def test_inject_codex_deliver_false_is_not_live(self, monkeypatch):
        _patch_registry(monkeypatch, [_FakeEntry("codex", "cx-9", "live")])
        import fno.agents.dispatch as dispatch

        monkeypatch.setattr(dispatch, "_deliver_live", lambda *a, **k: False)
        assert inject_pr_merged("cx-9", 42, "codex") == (False, "not-live")

    def test_inject_gemini_unsupported(self, monkeypatch):
        delivered, reason = inject_pr_merged("gm-1", 42, "gemini")
        assert delivered is False
        assert reason.startswith("unsupported-harness")
