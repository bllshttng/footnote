"""Bus-only delivery policy conformance (x-e21e) -- the no-paste pin.

The operator's live specimens: two injections split their sentences mid-compose
inside the reports of the defect itself. The fix is a recipient-level
``delivery_policy: bus-only`` row stamp: mail to that recipient never
prompt-line injects, always queues durable, and the receipt says the policy --
never a live-miss, never a stranded message, never a liveness verdict (the
naming trap that renamed NOT_INJECTABLE off "not-live", mail_inject.rs:96).

These tests pin the two VERIFY cases from the node plus the surrounding
contract: the no-paste guarantee, the worker-still-pastes counterpart, the
receipt vocabulary, the escalation skip, the raw-lane refusal, and the
register upsert's preserve-when-silent stamp.
"""
from __future__ import annotations

import json

import pytest
from typer.testing import CliRunner

from fno.cli import app
from fno.paths_testing import use_tmpdir

BUS_SID = "7c19d2e4-0b3f-4c5a-9e88-2ad4f0c81b97"
WORKER_SID = "3f8a51c0-6d22-4b7e-b1a4-90ce7d3f25aa"
# register_existing_session's axis-legacy kwarg still spells `provider`; pass
# the harness through a name (the production caller's `provider=harness`
# pattern) so no provider-named binding holds a harness literal.
HARNESS = "claude"


@pytest.fixture
def runner():
    return CliRunner()


@pytest.fixture
def mailbox(tmp_path, monkeypatch):
    """Co-isolate the bus log, inbox, registry, and every discovery source."""
    monkeypatch.delenv("FNO_BUS_DIR", raising=False)
    monkeypatch.setenv("FNO_INBOX_ROOT", str(tmp_path))
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents import discover

    empty = tmp_path / "empty-discovery"
    empty.mkdir(exist_ok=True)
    for env in (
        discover.SESSIONS_DIR_ENV,
        discover.PROJECTS_DIR_ENV,
        discover.CODEX_SESSIONS_DIR_ENV,
        discover.OPENCODE_STORAGE_DIR_ENV,
    ):
        monkeypatch.setenv(env, str(empty))
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(tmp_path / "daemon-empty"))
    return tmp_path


def _seed_registry(*policies: dict) -> None:
    """Write registry rows; each kwarg-dict maps AgentEntry kwargs."""
    from fno.agents.registry import AgentEntry, write_registry

    rows = []
    for spec in policies:
        rows.append(
            AgentEntry(
                name=spec["name"],
                cwd="/tmp/x",
                log_path="/tmp/x.log",
                harness="claude",
                harness_session_id=spec["sid"],
                short_id=spec["sid"][:8],
                status=spec.get("status", "idle"),
                delivery_policy=spec.get("delivery_policy"),
            )
        )
    write_registry(rows)


def _boom_transport(monkeypatch):
    """The layer BELOW the gate: raise only when a subprocess would reach a
    prompt line (the mail-inject binary, a mux pane send). Discovery and other
    unrelated lanes legitimately shell the binary during a send, so the boom
    keys on the PASTE argv, not on subprocess use itself. Patching the
    injectors themselves would replace the gate under test -- the exact
    mistake a no-paste assertion must not make."""
    import subprocess as real_subprocess

    # Capture BEFORE the patch: setattr on the module attribute would make
    # real_subprocess.run resolve back to the patched function (infinite
    # recursion) since subprocess is a shared module object.
    _real_run = real_subprocess.run

    def _watch_run(argv, *args, **kwargs):
        joined = " ".join(str(a) for a in argv[:4])
        if "mail-inject" in joined or "pane" in joined:
            raise AssertionError(
                f"a prompt-line paste was attempted on a bus-only lane: {joined}"
            )
        return _real_run(argv, *args, **kwargs)

    monkeypatch.setattr(
        "fno.agents.dispatch.subprocess.run", _watch_run, raising=True
    )


def _drain_as(runner, monkeypatch, session_id):
    from fno.harness_identity import HARNESS_SESSION_MARKERS

    for marker, _harness in HARNESS_SESSION_MARKERS:
        monkeypatch.delenv(marker, raising=False)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", session_id)
    res = runner.invoke(app, ["mail", "drain-self", "--json"])
    assert res.exit_code == 0, res.output
    return json.loads(res.stdout.strip().splitlines()[-1])


# ---------------------------------------------------------------------------
# VERIFY case 1: a bus-only recipient receives NO prompt-line paste.
# ---------------------------------------------------------------------------


def test_bus_only_send_never_pastes_and_queues_durable(runner, mailbox, monkeypatch):
    _seed_registry({"name": "leader", "sid": BUS_SID,
                    "delivery_policy": "bus-only"})
    _boom_transport(monkeypatch)

    res = runner.invoke(
        app, ["mail", "send", BUS_SID, "probe", "--from-name", "peer"]
    )

    assert res.exit_code == 0, res.output
    assert "queued (durable)" in res.output
    assert "bus-only" in res.output
    # Not a miss, not stranded: the receipt names the polling contract.
    assert "live-miss" not in res.output
    assert "not live" not in res.output


def test_bus_only_queue_surfaces_at_the_recipients_drain(runner, mailbox, monkeypatch):
    _seed_registry({"name": "leader", "sid": BUS_SID,
                    "delivery_policy": "bus-only"})
    _boom_transport(monkeypatch)

    res = runner.invoke(
        app, ["mail", "send", BUS_SID, "probe", "--from-name", "peer"]
    )
    assert res.exit_code == 0, res.output

    drained = _drain_as(runner, monkeypatch, BUS_SID)
    assert drained, "a bus-only queue must drain -- it is delivery, not a strand"
    assert any("probe" in str(m) for m in drained)


def test_bus_only_send_skips_the_human_escalation(runner, mailbox, monkeypatch):
    _seed_registry({"name": "leader", "sid": BUS_SID,
                    "delivery_policy": "bus-only"})
    _boom_transport(monkeypatch)

    def _must_not_escalate(*_a, **_k):
        raise AssertionError("a bus-only queue escalated as if it were a miss")

    monkeypatch.setattr("fno.mail.cli._escalate_to_human", _must_not_escalate)

    res = runner.invoke(
        app, ["mail", "send", BUS_SID, "probe", "--from-name", "peer"]
    )
    assert res.exit_code == 0, res.output
    assert "escalated" not in res.output


# ---------------------------------------------------------------------------
# VERIFY case 2: a worker (no policy) still receives the prompt-line paste.
# ---------------------------------------------------------------------------


def test_worker_without_policy_still_injects_live(runner, mailbox, monkeypatch):
    _seed_registry({"name": "worker-a", "sid": WORKER_SID})

    attempts: list[str] = []

    def _inject(recipient, _text, **_k):
        attempts.append(recipient)
        return True

    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", _inject)

    res = runner.invoke(
        app, ["mail", "send", WORKER_SID, "probe", "--from-name", "peer"]
    )

    assert res.exit_code == 0, res.output
    assert attempts, "the worker's inject lane was never consulted"
    assert "delivered (hosted)" in res.output


# ---------------------------------------------------------------------------
# The raw lane: never queues durable, so a bus-only recipient is refused loud.
# ---------------------------------------------------------------------------


def test_raw_send_to_bus_only_is_refused_loud(runner, mailbox, monkeypatch):
    _seed_registry({"name": "leader", "sid": BUS_SID,
                    "delivery_policy": "bus-only"})
    _boom_transport(monkeypatch)

    res = runner.invoke(
        app, ["mail", "send", BUS_SID, "/code-review", "--raw", "--from-name", "peer"]
    )

    assert res.exit_code == 2, res.output
    assert "bus-only" in res.output
    assert "queued (durable)" not in res.output


def test_raw_check_answers_not_injectable_for_bus_only(runner, mailbox, monkeypatch):
    _seed_registry({"name": "leader", "sid": BUS_SID,
                    "delivery_policy": "bus-only"})
    _boom_transport(monkeypatch)

    res = runner.invoke(
        app,
        ["mail", "send", BUS_SID, "/code-review", "--raw", "--check",
         "--from-name", "peer"],
    )

    assert res.exit_code == 1, res.output
    assert "not-injectable" in res.output
    assert "bus-only" in res.output


# ---------------------------------------------------------------------------
# The naming trap, asserted the same way mail_inject.rs:1111 pins NOT_INJECTABLE.
# ---------------------------------------------------------------------------


def test_the_policy_token_is_a_delivery_fact_not_a_liveness_verdict():
    from fno.agents.dispatch import BUS_ONLY_POLICY

    assert BUS_ONLY_POLICY == "bus-only"
    for liveness_word in ("live", "dead", "idle", "busy", "asleep"):
        assert liveness_word not in BUS_ONLY_POLICY, (
            f"the policy token reads as a liveness verdict: {BUS_ONLY_POLICY!r}"
        )


# ---------------------------------------------------------------------------
# The register seam: stamp, preserve-when-silent, clear.
# ---------------------------------------------------------------------------


def test_register_stamps_bus_only_and_flagless_reregister_preserves(monkeypatch):
    from fno.agents.registry import (
        SCHEMA_VERSION,
        load_registry,
        register_existing_session,
    )

    entry = register_existing_session(
        provider=HARNESS,
        session_id=BUS_SID,
        cwd="/tmp/x",
        origin="operator",
        delivery_policy="bus-only",
    )
    assert entry.delivery_policy == "bus-only"
    assert SCHEMA_VERSION == 14

    # A re-firing SessionStart hook (no policy kwarg) must not clobber the
    # stamp -- the operator would silently revert to injectable.
    entry = register_existing_session(
        provider=HARNESS, session_id=BUS_SID, cwd="/tmp/x", origin="operator"
    )
    assert entry.delivery_policy == "bus-only", (
        "a flagless re-register reverted a bus-only stamp"
    )

    # The explicit clear.
    entry = register_existing_session(
        provider=HARNESS,
        session_id=BUS_SID,
        cwd="/tmp/x",
        origin="operator",
        delivery_policy="off",
    )
    assert entry.delivery_policy is None

    # The stamp round-trips through the store.
    register_existing_session(
        provider=HARNESS,
        session_id=BUS_SID,
        cwd="/tmp/x",
        origin="operator",
        delivery_policy="bus-only",
    )
    rows = [e for e in load_registry() if e.harness_session_id == BUS_SID]
    assert rows and rows[0].delivery_policy == "bus-only"


def test_register_rejects_an_unknown_policy(runner, mailbox):
    res = runner.invoke(app, ["agents", "register", "--delivery-policy", "always"])
    assert res.exit_code == 2, res.output
    assert "bus-only" in res.output


def test_the_gate_resolves_every_address_tier(monkeypatch):
    """The gate must recognize a bus-only row by session id, short id, AND name:
    a guard keyed on one tier is a guard on one of N paths (pitfall 1)."""
    from fno.agents.dispatch import BUS_ONLY_POLICY, _delivery_policy_refusal
    from fno.agents.registry import AgentEntry, write_registry

    write_registry([
        AgentEntry(
            name="leader",
            cwd="/tmp/x",
            log_path="/tmp/x.log",
            harness="claude",
            harness_session_id=BUS_SID,
            short_id=BUS_SID[:8],
            delivery_policy="bus-only",
        )
    ])
    for token in (BUS_SID, BUS_SID[:8], "leader"):
        assert _delivery_policy_refusal(token) == BUS_ONLY_POLICY, (
            f"the gate missed the {token!r} address tier"
        )
    assert _delivery_policy_refusal(WORKER_SID) is None
