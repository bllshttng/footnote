"""The keeper mail lane (x-0ea6): mail to a keeper-hosted lane-B thread.

The delivery-matrix file pins the whole name lane GIVEN an injector's answer;
this file adds the third transport. A keeper-hosted thread has neither lane-A
socket (claude control.sock, codex app-server), so before x-0ea6 it live-missed
on every rung and demoted to durable with a live thread right there. The
keeper's own unix socket is its transport, resolved by the verb from the
registry row the send already resolved the recipient from.

Test doubles sit at the same boundaries as test_mail_delivery_matrix.py: the
injector boundary for the ladder, and subprocess for the verb itself. These
tests pin what the CLI does GIVEN the verb's answer, never the keeper's own
correctness (that lives in the Rust suite's mail_inject tests).
"""
from __future__ import annotations

import json
import subprocess
from pathlib import Path

import pytest

from fno.agents.discover import DiscoveredSession
from fno.paths_testing import use_tmpdir

KEEPER_SID = "pi-thread-4f0ea6c1"
KEEPER_HANDLE = "pi-worker"


def _keeper_session() -> DiscoveredSession:
    return DiscoveredSession(
        session_id=KEEPER_SID,
        short_id=KEEPER_HANDLE,
        handle=KEEPER_HANDLE,
        pid=0,
        cwd="/repo",
        project=None,
        status="live",
        agent="pi",
    )


@pytest.fixture
def mailbox(tmp_path, monkeypatch):
    """Co-isolate the bus log and the inbox, same as the matrix file."""
    monkeypatch.delenv("FNO_BUS_DIR", raising=False)
    monkeypatch.setenv("FNO_INBOX_ROOT", str(tmp_path))
    use_tmpdir(monkeypatch, tmp_path)
    return tmp_path


def _send(monkeypatch, session, injector, capsys):
    """Drive the name-lane choke point directly with a resolved recipient."""
    from fno.mail import cli

    recorded: list[tuple] = []

    def _keeper(recipient, text, *, harness, **_k):
        recorded.append((recipient, text, harness))
        return injector(recipient, text, harness=harness, **_k)

    monkeypatch.setattr("fno.agents.dispatch._mail_inject_keeper", _keeper)
    # Lane A must stay untouched (AC3-EDGE): a call to either is a routing bug.
    def _wrong(_r, _t, **_k):
        raise AssertionError("lane-A injector fired for a keeper-lane recipient")

    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", _wrong)
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_codex", _wrong)
    monkeypatch.setattr("fno.agents.dispatch._mux_pane_send", _wrong)

    cli._name_lane_send("ping", from_name="web", resolved=session)
    return recorded, capsys.readouterr()


def test_a_keeper_recipient_routes_to_the_keeper_verb_and_reads_delivered(
    mailbox, monkeypatch, capsys
):
    """AC1: the receipt reads delivered, never queued (durable), never typed."""

    def _deliver(*_a, **_k):
        return True

    recorded, out = _send(monkeypatch, _keeper_session(), _deliver, capsys)

    assert recorded, "the keeper rung was never attempted"
    recipient, text, harness = recorded[0]
    assert recipient == KEEPER_SID
    assert harness == "pi", "the hosted harness names the settle-delay row"
    assert "<fno_mail" in text, "the keeper lane carries the wrapped envelope"
    assert "delivered (hosted)" in out.out
    assert "queued (durable)" not in out.out
    assert "typed (pane" not in out.out


def test_a_keeper_miss_demotes_durably_naming_the_verb_reason(
    mailbox, monkeypatch, capsys
):
    """AC2: an unreachable keeper demotes honestly and states why. The roster
    pane rung sits this out: the row hosts no pane by design, and a stale ref
    would type into an unrelated pane and read as delivered."""

    def _miss(*_a, reason_out, **_k):
        if reason_out is not None:
            reason_out.append("no-keeper-listener")
        return False

    def _resolve_agent(_sid):
        raise AssertionError("the roster pane rung fired for a keeper recipient")

    monkeypatch.setattr("fno.agents.registry.resolve_agent", _resolve_agent)

    _recorded, out = _send(monkeypatch, _keeper_session(), _miss, capsys)

    assert "delivered" not in out.out
    assert "queued (durable)" in out.out
    assert "[no-keeper-listener]" in out.out, "the receipt must name why it demoted"


def test_lane_a_recipients_never_touch_the_keeper_verb(mailbox, monkeypatch, capsys):
    """AC3-EDGE: claude and codex keep their existing transports."""
    from fno.mail import cli

    for harness, injector in (
        ("claude", "_mail_inject_claude"),
        ("codex", "_mail_inject_codex"),
    ):
        fired: list[str] = []
        monkeypatch.setattr(
            f"fno.agents.dispatch.{injector}",
            lambda _r, _t, **_k: (fired.append(injector), True)[1],
        )

        def _wrong(_r, _t, **_k):
            raise AssertionError("the keeper rung fired for a lane-A recipient")

        monkeypatch.setattr("fno.agents.dispatch._mail_inject_keeper", _wrong)
        session = DiscoveredSession(
            session_id="sid-" + harness,
            short_id=harness,
            handle=harness,
            pid=0,
            cwd="/repo",
            project=None,
            status="live",
            agent=harness,
        )
        cli._name_lane_send("ping", from_name="web", resolved=session)
        out = capsys.readouterr()
        assert fired == [injector], harness
        assert "delivered (hosted)" in out.out, harness


def test_an_unknown_harness_keeps_the_fallthrough_lanes(mailbox, monkeypatch, capsys):
    """The capability table raises on a name it does not know. A row carrying
    such a name keeps its pre-keeper behavior (roster rung, durable floor)
    rather than the send crashing on the way down."""

    def _wrong(_r, _t, **_k):
        raise AssertionError("the keeper rung fired for an unknown harness")

    monkeypatch.setattr("fno.agents.dispatch._mail_inject_keeper", _wrong)
    from fno.agents.registry import AgentResolutionError

    def _resolve_agent(_sid):
        raise AgentResolutionError("no row")

    monkeypatch.setattr("fno.agents.registry.resolve_agent", _resolve_agent)
    from fno.mail import cli

    session = DiscoveredSession(
        session_id="sid-weird",
        short_id="weird",
        handle="weird",
        pid=0,
        cwd="/repo",
        project=None,
        status="live",
        agent="notaharness",
    )
    cli._name_lane_send("ping", from_name="web", resolved=session)
    out = capsys.readouterr()
    assert "queued (durable)" in out.out


# ---------------------------------------------------------------------------
# The verb boundary itself: argv shape and the reason side-channel.
# ---------------------------------------------------------------------------


def _fake_verb(monkeypatch, stdout: str):
    calls: list[list[str]] = []
    real_run = subprocess.run

    def _run(argv, **_k):
        # Only the verb's own call is captured; the registry read that gates
        # bus-only policy shells out for its paths and runs unimpeded.
        if "mail-inject" not in argv:
            return real_run(argv, **_k)
        calls.append(list(argv))
        return subprocess.CompletedProcess(args=argv, returncode=0, stdout=stdout, stderr="")

    monkeypatch.setattr("fno.rust_binary.resolve_installed_binary", lambda: "/fake/fno-agents")
    monkeypatch.setattr("fno.agents.dispatch.subprocess.run", _run)
    return calls


def test_the_keeper_verb_argv_carries_the_hosted_harness_and_no_delay(monkeypatch):
    """The verb defaults --enter-delay-ms from the hosted harness's own
    contract row, so the Python side passes no delay: one fact, one source."""
    import fno.agents.dispatch as d

    calls = _fake_verb(monkeypatch, '{"delivered": true, "reason": "delivered"}')
    assert d._mail_inject_keeper(KEEPER_SID, "<fno_mail>hi</fno_mail>", harness="pi")
    argv = calls[0]
    assert argv[:2] == ["/fake/fno-agents", "mail-inject"]
    assert argv[argv.index("--session") + 1] == KEEPER_SID
    assert argv[argv.index("--harness") + 1] == "pi"
    assert "--enter-delay-ms" not in argv, "the verb resolves the delay itself"


def test_the_keeper_verb_miss_records_the_reason_token(monkeypatch):
    import fno.agents.dispatch as d

    _fake_verb(monkeypatch, '{"delivered": false, "reason": "no-keeper-listener"}')
    reasons: list = []
    assert not d._mail_inject_keeper(KEEPER_SID, "x", harness="pi", reason_out=reasons)
    assert reasons == ["no-keeper-listener"]


def test_a_bus_only_keeper_recipient_is_refused_before_the_binary(monkeypatch):
    import fno.agents.dispatch as d

    monkeypatch.setattr(d, "_delivery_policy_refusal", lambda _t: d.BUS_ONLY_POLICY)

    def _run(_argv, **_k):
        raise AssertionError("a bus-only recipient reached the binary")

    monkeypatch.setattr("fno.rust_binary.resolve_installed_binary", lambda: "/fake/fno-agents")
    monkeypatch.setattr("fno.agents.dispatch.subprocess.run", _run)
    reasons: list = []
    assert not d._mail_inject_keeper(KEEPER_SID, "x", harness="pi", reason_out=reasons)
    assert reasons == [d.BUS_ONLY_POLICY]


# ---------------------------------------------------------------------------
# The registered-name rung: _deliver_live routes a keeper row to the verb and
# never to the daemon RPC (whose "agent.deliver" knows nothing about a thread).
# ---------------------------------------------------------------------------


def test_deliver_live_routes_a_keeper_row_to_the_verb_not_the_daemon(monkeypatch):
    import fno.agents.dispatch as d
    from fno.agents.registry import AgentEntry

    fired: list[tuple[str, str]] = []
    monkeypatch.setattr(
        d,
        "_mail_inject_keeper",
        lambda recipient, text, *, harness, **_k: (
            fired.append((recipient, harness)),
            True,
        )[1],
    )

    def _wrong(*_a, **_k):
        raise AssertionError("the daemon RPC fired for a keeper-lane row")

    monkeypatch.setattr(d, "_daemon_rpc", _wrong)
    monkeypatch.setattr(d, "_mux_pane_send", _wrong)

    entry = AgentEntry(
        name="wk-9",
        harness="pi",
        cwd="/repo",
        log_path="",
        harness_session_id=KEEPER_SID,
        messaging_socket_path="/x/.fno/mux/threads/wk-9.sock",
    )
    assert d._deliver_live(entry, "hi", "web")
    assert fired == [(KEEPER_SID, "pi")]


def test_deliver_live_miss_names_the_verb_reason(monkeypatch):
    import fno.agents.dispatch as d
    from fno.agents.registry import AgentEntry

    monkeypatch.setattr(
        d,
        "_mail_inject_keeper",
        lambda _r, _t, *, reason_out, **_k: (
            reason_out.append("no-keeper-listener"),
            False,
        )[1],
    )

    entry = AgentEntry(
        name="wk-9",
        harness="pi",
        cwd="/repo",
        log_path="",
        harness_session_id=KEEPER_SID,
        messaging_socket_path="/x/.fno/mux/threads/wk-9.sock",
    )
    reasons: list = []
    assert not d._deliver_live(entry, "hi", "web", reason_out=reasons)
    assert reasons == ["no-keeper-listener"]


def test_deliver_live_leaves_a_socketless_keeper_row_on_its_fallthrough(
    monkeypatch,
):
    """A keeper-lane row with no keeper socket (pre-spawn, or a cleared field)
    is not the verb's to refuse: it keeps the daemon-RPC fall-through so the
    demotion names that lane's cause, never a fabricated not-injectable."""

    def _keeper(*_a, **_k):
        raise AssertionError("the verb fired for a row with no keeper socket")

    monkeypatch.setattr("fno.agents.dispatch._mail_inject_keeper", _keeper)
    import fno.agents.dispatch as d
    from fno.agents.registry import AgentEntry

    monkeypatch.setattr(
        d, "_daemon_rpc", lambda *_a, **_k: None
    )  # prints its own demotion notice
    entry = AgentEntry(
        name="wk-9", harness="pi", cwd="/repo", log_path="", harness_session_id=KEEPER_SID
    )
    assert not d._deliver_live(entry, "hi", "web")


# ---------------------------------------------------------------------------
# The journey (plan verification 3): the real CLI, the real verb binary, a real
# keeper socket, and a scripted hosted TUI that stands in for the one thing this
# node must not spend: a live provider session. The scripted TUI speaks the
# keeper frame protocol exactly (u8 tag | u32 LE len | payload) and does what
# pi's TUI does when a submitted turn lands: appends the turn to the cwd-scoped
# session file. Everything between `mail send` and that file is production code.
# ---------------------------------------------------------------------------

_WORKSPACE = Path(__file__).resolve().parents[3] / "crates" / "fno-agents"


def _serve_scripted_tui(sock_path, transcript_path):
    """Serve the keeper socket: record Input frames, file the turn on the CR.

    The only faked thing is the hosted harness's own transcript write - the
    production confirm greps exactly this file for the envelope's marker line,
    so a green confirm here is the real content-confirm path running."""
    import socket as socket_mod
    import struct
    import threading

    sock_path.parent.mkdir(parents=True, exist_ok=True)
    server = socket_mod.socket(socket_mod.AF_UNIX, socket_mod.SOCK_STREAM)
    server.bind(str(sock_path))
    server.listen(1)

    def _run():
        try:
            conn, _ = server.accept()
        except OSError:
            return  # torn down at teardown before any client arrived
        buf = b""
        paste = b""
        try:
            while True:
                chunk = conn.recv(4096)
                if not chunk:
                    return
                buf += chunk
                while len(buf) >= 5:
                    tag = buf[0]
                    (length,) = struct.unpack("<I", buf[1:5])
                    if len(buf) < 5 + length:
                        break
                    payload = buf[5 : 5 + length]
                    buf = buf[5 + length :]
                    if tag != 1:  # TAG_INPUT; the inject path sends only Input
                        continue
                    if payload == b"\r":
                        transcript_path.parent.mkdir(parents=True, exist_ok=True)
                        with open(transcript_path, "a", encoding="utf-8") as fh:
                            fh.write(json.dumps({"text": paste.decode()}) + "\n")
                    else:
                        paste = payload
        finally:
            conn.close()

    threading.Thread(target=_run, daemon=True).start()
    return server


def test_the_journey_mail_send_delivers_to_a_live_keeper_hosted_thread(
    tmp_path, monkeypatch
):
    import os
    import shutil

    binary = _WORKSPACE / "target" / "debug" / "fno-agents"
    if not binary.exists():
        pytest.skip("fno-agents debug binary not built; run cargo build -p fno-agents")

    # A keeper socket path must stay under AF_UNIX's 104-byte sun_path limit,
    # which both pytest's tmp tree and the conftest's HOME sandbox exceed, so
    # this test keeps its own short home and pins the ONE registry resolver to
    # it: Python resolves agents_registry_path() from HOME, the verb (a
    # subprocess) from FNO_AGENTS_HOME, and they must name the same file.
    short = Path("/tmp") / f"fno-journey-{os.getpid()}"
    shutil.rmtree(short, ignore_errors=True)
    (short / ".fno" / "agents").mkdir(parents=True)
    use_tmpdir(monkeypatch, tmp_path)  # the settings-cache wrappers
    monkeypatch.delenv("FNO_BUS_DIR", raising=False)
    monkeypatch.setenv("FNO_INBOX_ROOT", str(tmp_path / "inbox"))

    import fno.paths

    registry_file = short / ".fno" / "agents" / "registry.json"
    monkeypatch.setattr(fno.paths, "agents_registry_path", lambda: registry_file)
    monkeypatch.setenv("FNO_AGENTS_HOME", str(short / ".fno" / "agents"))
    pi_home = short / "pihome"
    monkeypatch.setenv("PI_HOME", str(pi_home))
    try:
        _run_journey(short, pi_home, binary, monkeypatch)
    finally:
        shutil.rmtree(short, ignore_errors=True)


def _run_journey(short, pi_home, binary, monkeypatch):
    # A real lane-B row, registered through the production spawn entry with
    # the same doubles the lane-B spawn tests use (no keeper binary launch).
    import fno.agents.dispatch as dispatch_mod
    from fno.agents.dispatch import _lane_b_thread_spawn
    from fno.agents.registry import load_registry

    class _FakeProc:
        pid = 4242

        def kill(self) -> None:  # pragma: no cover - failure paths only
            pass

    _argv_holder: dict[str, object] = {}

    def _fake_popen(argv, **_k):
        _argv_holder["argv"] = argv
        return _FakeProc()

    def _fake_identify(_sock, _timeout=10.0):
        argv = list(_argv_holder["argv"])
        session_id = argv[argv.index("--pane-key") + 1]
        return {
            "v": 1,
            "keeper_pid": 4242,
            "child_pid": 555,
            "session_id": session_id,
            "argv": argv[argv.index("--") + 1 :],
            "cwd": str(short),
        }

    # Scoped by hand, not monkeypatch: the Popen double patches the SHARED
    # stdlib module, and this test must run the real verb through
    # subprocess.run afterward (whose `with Popen(...)` would hit the fake).
    real_popen = dispatch_mod.subprocess.Popen
    dispatch_mod.subprocess.Popen = _fake_popen
    try:
        monkeypatch.setattr(dispatch_mod, "_keeper_identify", _fake_identify)
        monkeypatch.setattr(
            dispatch_mod, "_lane_b_worker_binary", lambda: Path("/fake/fno-agents-worker")
        )
        receipt = _lane_b_thread_spawn(name="wk-journey", harness="pi", cwd=short)
    finally:
        dispatch_mod.subprocess.Popen = real_popen

    row = next(e for e in load_registry() if e.name == "wk-journey")
    assert row.harness == "pi"
    assert row.messaging_socket_path == receipt["keeper_socket"]

    # pi writes its session file at the first turn attempt; the empty cwd dir
    # is the PendingStore shape the confirm handles.
    encoded = "--" + str(short).lstrip("/").replace("/", "-") + "--"
    sessions_dir = pi_home / "agent" / "sessions" / encoded
    sessions_dir.mkdir(parents=True, exist_ok=True)
    transcript = sessions_dir / f"20260901T000000Z_{receipt['session_id']}.jsonl"

    server = _serve_scripted_tui(Path(receipt["keeper_socket"]), transcript)
    try:
        monkeypatch.setattr("fno.rust_binary.resolve_installed_binary", lambda: binary)
        from typer.testing import CliRunner

        from fno.cli import app

        res = CliRunner().invoke(
            app, ["agents", "mail", "send", "wk-journey", "ping", "--from-name", "web"]
        )
    finally:
        server.close()

    assert res.exit_code == 0, res.output
    assert "delivered (hosted)" in res.output, res.output
    assert "queued (durable)" not in res.output
    assert "typed (pane" not in res.output
    landed = transcript.read_text(encoding="utf-8")
    assert "<fno_mail" in landed, "the scripted TUI saw no envelope"
    assert "ping" in landed
    # The turn was filed as the enqueue-record shape the content confirm reads.
    assert json.loads(landed.strip().splitlines()[-1])["text"].startswith("\x1b[200~<fno_mail")
