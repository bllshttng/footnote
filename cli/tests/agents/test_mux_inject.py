"""Mail live-inject dispatch on the registry mux ref (4a-G2/G3, task 4.9/4.10).

Node x-1904 de-vetoed the mail-delivery mux lane: it no longer routes through
`fno mux pane send --guarded`. A busy claude session enqueues an injected
paste rather than corrupting its composer (measured, not inferred; see
`crates/fno/src/server.rs`), so the server-side turn-taken interlock that
refused a mid-turn recipient before any byte was written vetoed exactly the
delivery this transport can make. Mail delivery now pastes unguarded and
confirms by content against the recipient's own transcript
(`test_mux_pane_guarded.py` in `tests/unit/` covers that confirm path in
detail); this file keeps the `guarded=True` mechanism tests below because the
underlying capability is still real (the rerun verb still wants it -- refusing
mid-turn is the conservative call for a rerun, unlike mail), just no longer
what mail-delivery routing exercises. A guarded send does NOT hold the writer
claim -- the server guard reads any live claim holder as `busy: relay`, so
holding our own claim would self-block every guarded send. The unguarded
peer-follow-up lane still holds the claim around the text-then-CR burst.
The mux subprocess is faked; the real socket path is the agent_edge e2e.
"""
from __future__ import annotations

import subprocess
from pathlib import Path

import pytest

from fno.paths_testing import use_tmpdir


@pytest.fixture(autouse=True)
def _idle_pane(monkeypatch):
    """Every test in this module drives an IDLE pane.

    Node x-3a64 put a read-back gate in front of the enveloped pane lane: it
    reads the frame, asks the manifest engine whether an option prompt is
    showing, and refuses when it cannot measure. These tests stub
    ``subprocess.run`` wholesale, so the detector reads an empty frame and
    correctly refuses as unmeasurable -- which is the gate working, not the
    transport failing. Stub the verdict so each test keeps asserting the thing
    it was written for. The gate itself has its own tests in
    ``test_dispatch_mux_send.py``, which does NOT use this fixture.
    """
    monkeypatch.setattr(
        "fno.mail.pane_transport.prompt_refusal",
        lambda **_kwargs: None,
    )

def _mux_entry(name: str = "muxed", provider: str = "claude"):
    from fno.agents.registry import AgentEntry

    return AgentEntry(
        name=name,
        harness=provider,
        cwd="/w",
        # x-7bcd: a mux-hosted row that has not yet captured its harness
        # session id (the happy/claude id-less lane) still needs a
        # resolvable handle. Production mint (mux_spawn.py) now touches a
        # fallback log file for exactly this shape; this fixture mirrors
        # that, not a real tailed log.
        log_path="/w/muxed.log",
        status="live",
        mux={"session": "work", "pane_id": 7},
    )


class FakeMux:
    """Record `fno mux pane <verb> ...` calls; script per-verb exit codes.

    ``fail_stderr`` overrides the refusal text for a verb (the pane-claim gate
    reads it); ``fail_times`` fails a verb for the first N calls (a held claim
    that clears after a retry)."""

    def __init__(
        self,
        fail_verbs: set[str] | None = None,
        fail_stderr: dict[str, str] | None = None,
        fail_times: dict[str, int] | None = None,
    ) -> None:
        self.calls: list[tuple[list[str], str | None]] = []
        self.fail_verbs = fail_verbs or set()
        self.fail_stderr = fail_stderr or {}
        self.fail_times = fail_times or {}

    def __call__(self, argv, input=None, **kwargs):
        verb = argv[3]
        self.calls.append((list(argv), input))
        times = self.fail_times.get(verb)
        if times is not None and times > 0:
            self.fail_times[verb] = times - 1
        rc = 1 if verb in self.fail_verbs or (times is not None and times > 0) else 0
        stderr = (self.fail_stderr.get(verb) or "boom") if rc else ""
        return subprocess.CompletedProcess(argv, rc, "", stderr)


@pytest.fixture(autouse=True)
def _fast(monkeypatch):
    from fno.agents import dispatch as dispatch_mod

    monkeypatch.setattr(dispatch_mod.time, "sleep", lambda _s: None)


def _patch_mux(monkeypatch, fake: FakeMux) -> None:
    from fno.agents import dispatch as dispatch_mod

    monkeypatch.setattr(dispatch_mod.subprocess, "run", fake)


def test_guarded_inject_pastes_and_submits_without_a_claim(monkeypatch) -> None:
    # guarded=True (no mail-delivery caller passes it any more, node x-1904):
    # the server-side interlock is the authority, so the send never holds the
    # writer claim (which the guard would read as busy: relay and self-refuse).
    # Just paste --guarded, then CR.
    from fno.agents.dispatch import _mux_pane_send

    fake = FakeMux()
    _patch_mux(monkeypatch, fake)
    assert _mux_pane_send(_mux_entry(), "<fno_mail>hi</fno_mail>") is True

    verbs = [c[0][3] for c in fake.calls]
    assert verbs == ["send", "send"]
    for argv, _ in fake.calls:
        assert argv[argv.index("--session") + 1] == "work"
        assert argv[4] == "7"
    # Envelope bytes ride --stdin --guarded verbatim; the CR submit is its own send.
    paste, cr = fake.calls[0], fake.calls[1]
    assert "--stdin" in paste[0] and "--guarded" in paste[0]
    assert paste[1] == "<fno_mail>hi</fno_mail>"
    assert cr[0][cr[0].index("--text") + 1] == "\r"


def test_guarded_dead_pane_fails_closed_with_no_cr(monkeypatch) -> None:
    # guarded=True: a paste the server refuses (or a dead pane) returns False
    # with no CR submit -- and no claim/release, since a guarded send never
    # claims. Real behavior for the rerun caller; mail no longer reaches this.
    from fno.agents.dispatch import _mux_pane_send

    fake = FakeMux(fail_verbs={"send"})
    _patch_mux(monkeypatch, fake)
    assert _mux_pane_send(_mux_entry(), "hi") is False
    assert [c[0][3] for c in fake.calls] == ["send"], "no CR, no claim/release"


def test_unguarded_follow_up_claims_sends_and_releases(monkeypatch) -> None:
    # The peer-follow-up lane keeps its raw channel: hold the writer claim across
    # the text-then-CR burst, release after, and never pass --guarded.
    from fno.agents.dispatch import _mux_pane_send

    fake = FakeMux()
    _patch_mux(monkeypatch, fake)
    assert _mux_pane_send(_mux_entry(), "hi", guarded=False) is True
    assert [c[0][3] for c in fake.calls] == ["claim", "send", "send", "release"]
    assert "--guarded" not in fake.calls[1][0]


def test_unguarded_claim_refusal_is_fail_open(monkeypatch) -> None:
    # A pane spawned without --claim refuses the acquire; the unguarded send
    # proceeds and no release is issued.
    from fno.agents.dispatch import _mux_pane_send

    fake = FakeMux(fail_verbs={"claim"})
    _patch_mux(monkeypatch, fake)
    assert _mux_pane_send(_mux_entry(), "hi", guarded=False) is True
    verbs = [c[0][3] for c in fake.calls]
    assert verbs == ["claim", "send", "send"]


def test_held_writer_claim_is_waited_out_then_proceeds(monkeypatch) -> None:
    # x-4b0b review: a HELD claim means another writer is mid-burst on this
    # pane. The send waits for the burst to clear instead of pasting a second
    # envelope under the holder's (the settle window between paste and CR is
    # a single-writer window).
    from fno.agents.dispatch import _mux_pane_send

    fake = FakeMux(
        fail_times={"claim": 2},
        fail_stderr={"claim": "pane 7 writer claim held by pid 999"},
    )
    _patch_mux(monkeypatch, fake)
    assert _mux_pane_send(_mux_entry(), "hi", guarded=False) is True
    verbs = [c[0][3] for c in fake.calls]
    assert verbs == ["claim", "claim", "claim", "send", "send", "release"]


def test_held_writer_claim_past_the_wait_demotes_without_pasting(monkeypatch) -> None:
    # A claim still held past the wait demotes to durable: no paste, no CR,
    # no interleaved envelopes.
    from fno.agents import dispatch as dispatch_mod
    from fno.agents.dispatch import _mux_pane_send

    fake = FakeMux(
        fail_verbs={"claim"},
        fail_stderr={"claim": "pane 7 writer claim held by pid 999"},
    )
    _patch_mux(monkeypatch, fake)
    monkeypatch.setattr(dispatch_mod, "_MUX_PANE_CLAIM_WAIT_S", 0.0)
    assert _mux_pane_send(_mux_entry(), "hi", guarded=False) is False
    verbs = [c[0][3] for c in fake.calls]
    assert verbs == ["claim"]


def test_held_claim_marker_is_pinned_on_both_sides_of_the_contract() -> None:
    """The pane-claim gate greps the server's refusal stderr for a phrase. Pin
    the phrase at BOTH ends so a wording change on either side fails a test
    instead of silently degrading the gate back to fail-open (the fakes in the
    tests above hardcode the same string, so they alone cannot detect drift).
    If the refusal messages ever localize, replace this grep with a dedicated
    held-claim exit code and branch on that."""
    from pathlib import Path

    root = Path(__file__).resolve().parents[3]
    server = (root / "crates/fno/src/server.rs").read_text(encoding="utf-8")
    python = (root / "cli/src/fno/agents/dispatch.py").read_text(encoding="utf-8")
    assert "writer claim held by pid" in server
    assert '_MUX_CLAIM_HELD_MARKER = "held by pid"' in python


def test_mux_pane_send_uses_the_target_harness_submit_delay(monkeypatch) -> None:
    from fno.agents import dispatch as dispatch_mod
    from fno.agents import harness_map

    fake = FakeMux()
    _patch_mux(monkeypatch, fake)
    sleeps = []
    monkeypatch.setattr(dispatch_mod.time, "sleep", sleeps.append)
    original = harness_map.capabilities

    def caps(harness):
        value = dict(original(harness))
        value["send_keys_enter_delay_ms"] = 125
        value["submit_keys"] = ["enter"]
        return value

    monkeypatch.setattr(harness_map, "capabilities", caps)
    assert dispatch_mod._mux_pane_send(_mux_entry(), "hi") is True
    assert sleeps == [0.125]
    assert fake.calls[1][0][fake.calls[1][0].index("--text") + 1] == "\r"


@pytest.mark.parametrize("harness", ["gemini", "opencode"])
def test_mux_pane_send_refuses_unpinned_submit_contract_without_writing(
    harness: str, monkeypatch
) -> None:
    """A harness with no pinned submit contract is refused BEFORE any pane bytes.

    codex used to stand in for "unpinned" here and no longer can: its contract is
    pinned to ["enter"], measured against 0.148.0. The two that remain are
    unpinned because nothing has been measured for them, not because their panes
    are known to be unreachable.
    """
    from fno.agents.dispatch import _mux_pane_send

    fake = FakeMux()
    _patch_mux(monkeypatch, fake)
    assert _mux_pane_send(_mux_entry("muxed", harness), "hi") is False
    assert fake.calls == []


def test_mux_pane_send_delivers_to_a_codex_pane(monkeypatch, capsys) -> None:
    """The correction this node exists for: mail REACHES a codex pane. It landed
    in the composer and was never submitted, because a capability table refused
    the lane before the transport was tried. With submit_keys pinned, the text
    and the carriage return both go."""
    from fno.agents.dispatch import _mux_pane_send

    fake = FakeMux()
    _patch_mux(monkeypatch, fake)
    assert _mux_pane_send(_mux_entry("muxed", "codex"), "hi") is True

    verbs = [call[0][3] for call in fake.calls]
    assert "send" in verbs
    # The carriage return actually rides: without it the message renders under
    # "tab to queue message" and sits there unsent.
    written = "".join((call[1] or "") + " ".join(call[0]) for call in fake.calls)
    assert "\r" in written


def test_mux_pane_send_delivers_to_an_agy_pane(monkeypatch) -> None:
    """Measured agy pane behavior uses an Enter submit after the pasted text."""
    from fno.agents.dispatch import _mux_pane_send

    fake = FakeMux()
    _patch_mux(monkeypatch, fake)
    assert _mux_pane_send(_mux_entry("muxed", "agy"), "hi") is True

    assert [call[0][3] for call in fake.calls] == ["send", "send"]
    paste, submit = fake.calls
    assert "--stdin" in paste[0]
    # The body rides inside the envelope, not verbatim: an agent-to-agent pane
    # drive carries the same `<fno_mail>` attribution the mail lane produces
    # unless the caller opts out with raw=True. This test is about the SUBMIT
    # key, so it asserts the payload is carried rather than that it is bare.
    assert "hi" in paste[1]
    assert paste[1].startswith("<fno_mail")
    assert submit[0][submit[0].index("--text") + 1] == "\r"


def test_control_socket_inject_passes_the_same_contract_delay(monkeypatch) -> None:
    import json

    from fno import rust_binary
    from fno.agents import dispatch as dispatch_mod
    from fno.agents import harness_map

    monkeypatch.setattr(rust_binary, "resolve_installed_binary", lambda: Path("/bin/fno-agents"))
    # Hermetic + the unresolved-recipient case: no roster row for "session".
    monkeypatch.setattr(dispatch_mod, "_roster_entry_for_session", lambda session: None)
    original = harness_map.capabilities

    def caps(harness):
        value = dict(original(harness))
        value["send_keys_enter_delay_ms"] = 321
        return value

    monkeypatch.setattr(harness_map, "capabilities", caps)
    seen = []

    def run(argv, **kwargs):
        seen.append(list(argv))
        return subprocess.CompletedProcess(argv, 0, json.dumps({"delivered": True}), "")

    monkeypatch.setattr(dispatch_mod.subprocess, "run", run)
    assert dispatch_mod._mail_inject_claude("session", "<fno_mail>hi</fno_mail>")
    argv = next(argv for argv in seen if "mail-inject" in argv)
    assert argv[argv.index("--enter-delay-ms") + 1] == "321"


def test_control_socket_inject_delay_follows_the_recipient_harness(monkeypatch) -> None:
    """x-4b0b: the ``--enter-delay-ms`` argv value is the RECIPIENT's table row,
    not a claude constant. Three resolution paths, one expected value: the
    caller-held row (``harness=`` kwarg), the roster lookup by session id, and
    - patched to None by the sibling test above - the claude fallback."""
    import json
    from types import SimpleNamespace

    from fno import rust_binary
    from fno.agents import dispatch as dispatch_mod
    from fno.agents import harness_map

    monkeypatch.setattr(rust_binary, "resolve_installed_binary", lambda: Path("/bin/fno-agents"))
    original = harness_map.capabilities

    def caps(harness):
        value = dict(original(harness))
        value["send_keys_enter_delay_ms"] = 321 if harness == "claude" else 654
        return value

    monkeypatch.setattr(harness_map, "capabilities", caps)
    monkeypatch.setattr(
        dispatch_mod,
        "_roster_entry_for_session",
        lambda session: SimpleNamespace(harness="codex"),
    )
    seen = []

    def run(argv, **kwargs):
        seen.append(list(argv))
        return subprocess.CompletedProcess(argv, 0, json.dumps({"delivered": True}), "")

    monkeypatch.setattr(dispatch_mod.subprocess, "run", run)

    def _delay_of(call) -> str:
        seen.clear()
        assert dispatch_mod._mail_inject_claude("session", "<fno_mail>hi</fno_mail>", **call)
        argv = next(argv for argv in seen if "mail-inject" in argv)
        return argv[argv.index("--enter-delay-ms") + 1]

    # Roster lookup resolves the codex row.
    assert _delay_of({}) == "654"
    # The caller-held row wins without touching the roster.
    assert _delay_of({"harness": "codex"}) == "654"
    # An explicit claude row is honored, not overridden by the roster patch.
    assert _delay_of({"harness": "claude"}) == "321"


def _capture_audits(monkeypatch) -> list[tuple[dict, object]]:
    """Intercept the canonical fno.events append the audit floor uses."""
    import fno.events as events_mod

    seen: list[tuple[dict, object]] = []
    monkeypatch.setattr(
        events_mod,
        "append_event",
        lambda event, path=None, **kw: seen.append((event, path)),
    )
    return seen


def test_mux_pane_send_audits_raw_inject(monkeypatch) -> None:
    """AC10: the mux pane lane records an agent_raw_inject for an unwrapped
    payload (it never reaches the Rust mail-inject binary, so this site is
    mandatory, not decorative) and stays silent for a <fno_mail>-wrapped one.

    Since node x-3a64 an unwrapped payload only reaches the pane through
    ``raw=True``, the keystroke opt-out -- a default send is enveloped and so is
    audited by construction. That is the row keeping its meaning, not losing it:
    it still marks every send that arrives at a prompt with no attribution."""
    from fno.agents.dispatch import _mux_pane_send

    fake = FakeMux()
    _patch_mux(monkeypatch, fake)
    seen = _capture_audits(monkeypatch)

    # Unwrapped -> one audit record; wrapped envelope -> none.
    _mux_pane_send(_mux_entry(), "/code-review <level> --comment --fix", raw=True)
    _mux_pane_send(_mux_entry(), "<fno_mail from=\"a\">hi</fno_mail>")

    assert len(seen) == 1, "only the unwrapped payload is audited"
    event, path = seen[0]
    # The CANONICAL {type, source, data} envelope, not the flat {kind, ...} shape:
    # ~/.fno/agents/events.jsonl is canonical-only, and a consumer reading
    # data.target_session (where schema.yaml says it lives) must not miss this.
    assert event["type"] == "agent_raw_inject"
    assert event["source"] == "daemon"
    data = event["data"]
    assert data["payload"] == "/code-review <level> --comment --fix"
    assert data["lane"] == "mux-pane"
    assert data["harness"] == "claude"
    assert data["target_cwd"] == "/w"
    assert data["confirmed"] is True
    # The Python mux site writes the SAME log the Rust mail-inject binary uses
    # (~/.fno/agents/events.jsonl), so the audit floor is one file, not two.
    assert str(path).endswith("agents/events.jsonl")


def test_mux_pane_send_audit_records_a_failed_send_as_unconfirmed(monkeypatch) -> None:
    """No phantom records: a stalled pane still audits (the bytes may have
    landed) but says so, instead of asserting an injection that never happened."""
    from fno.agents.dispatch import _mux_pane_send

    _patch_mux(monkeypatch, FakeMux(fail_verbs={"send"}))
    seen = _capture_audits(monkeypatch)

    assert _mux_pane_send(_mux_entry(), "/code-review", raw=True) is False
    assert len(seen) == 1
    assert seen[0][0]["data"]["confirmed"] is False


def test_mux_pane_send_does_not_audit_cross_session_envelope(monkeypatch) -> None:
    """A wrapped <cross-session-message> peer follow-up carries its own
    agent-authored marker, so it must NOT be logged as a raw inject (regression:
    the guard admitted it because it does not start with <fno_mail)."""
    from fno.agents.dispatch import _mux_pane_send

    fake = FakeMux()
    _patch_mux(monkeypatch, fake)
    seen = _capture_audits(monkeypatch)

    _mux_pane_send(
        _mux_entry(),
        '<cross-session-message from-name="peer-x">\nstatus?\n</cross-session-message>',
    )

    assert not seen, (
        "the cross-session-message envelope is wrapped; it must not audit"
    )


def test_deliver_live_dispatches_on_mux_ref_before_legacy_lanes(
    tmp_path: Path, monkeypatch
) -> None:
    # Dual-run: a mux row (any provider) never touches the daemon RPC or the
    # control.sock lanes; a worker/bg row still does (AC3 + AC5-FR).
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents import dispatch as dispatch_mod

    seen = []
    monkeypatch.setattr(
        dispatch_mod,
        "_mux_pane_send",
        lambda entry, text, **_k: seen.append(entry.name) or True,
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
        name="wk", harness="codex", cwd="/w", log_path="", short_id="wk-1"
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
    # The body rides --stdin inside the cross-session-message container so the
    # pane reads it as a peer turn, not bare operator input.
    sent = fake.calls[1][1]
    assert "<cross-session-message from-name=" in sent
    assert "\nping\n" in sent


def test_ask_mux_preserves_from_name_framing(tmp_path: Path, monkeypatch) -> None:
    """A --from-name peer message keeps its attribution in the mux container."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed(_mux_entry())
    fake = FakeMux()
    _patch_mux(monkeypatch, fake)

    from fno.agents.dispatch import dispatch_ask

    dispatch_ask("muxed", "ping", provider=None, cwd=Path("/w"), from_name="peer-x")
    sent = fake.calls[1][1]
    assert '<cross-session-message from-name="peer-x">' in sent


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
    """A codex mux row rides pane send, as the name always said it should.

    It previously refused, because [harness.codex] declared
    submit_keys = ["unsupported"]. That was a declaration, not a measurement.
    """
    use_tmpdir(monkeypatch, tmp_path)
    _seed(_mux_entry(name="cmux", provider="codex"))
    fake = FakeMux()
    _patch_mux(monkeypatch, fake)

    from fno.agents.dispatch import dispatch_ask

    dispatch_ask("cmux", "ping", provider=None, cwd=Path("/w"))
    assert [call[0][3] for call in fake.calls], "the pane lane must be tried"
