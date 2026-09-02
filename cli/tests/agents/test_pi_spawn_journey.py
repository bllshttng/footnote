"""The spawn-seam journey (x-43bd): the PUBLIC surface reaches pi's
keeper-hosted thread lane, and the loop extension stops it.

This is the bar pi's caps row set when it flipped ``thread = true``: "when
that arm ships it brings its own journey for the lanes it opens." The
restart journey (wk-x61bc, test_thread_keeper_journey.py) drove
``_lane_b_thread_spawn`` directly, which is exactly why the spawn seam
stayed unproven. This journey enters through
``fno agents spawn -H pi --substrate thread`` and proves, with positive
markers only:

  1. the spawn registers a row whose ``harness_session_id`` is the minted
     id the receipt carried and whose ``messaging_socket_path`` answers
     Identify with the same id and child pid;
  2. that id matches an entry in PI'S OWN session store - never a proxy
     read, never a bare exit code;
  3. the seed the spawn carried actually landed in the worker (a live row
     with no orders is the strand this seam must not produce);
  4. a ``/target``-family message dispatched over the mail lane produces a
     fresh assistant turn, and the footnote loop extension shells
     ``fno-agents loop-check`` at the ``agent_settled`` boundary at least
     once - recorded via FNO_AGENTS_BIN pointing at a wrapper that logs the
     argv, then execs the real gate. The gate's DECISION is not asserted:
     loop-check is the sole completion authority, and what it decides about
     this synthetic manifest is its own business.

RUN OF RECORD, 2026-09-02, pi 0.84.2 on a live openai-codex subscription,
worktree debug builds: **GREEN** - see the PR description for x-43bd.

The live test is opt-in (``FNO_PI_LIVE=1``) because it spends real
subscription tokens and needs this machine's pi credentials. It installs
the shipped footnote.ts extension into the real ``~/.pi/agent/extensions``
(the install surface pi actually reads, and exactly what ``fno config
setup`` does), and restores whatever was there before.
"""
from __future__ import annotations

import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import pytest

from fno.agents.harnesses.pi import lookup_sessions
from fno.agents.registry import load_registry
from fno.paths_testing import use_tmpdir

LIVE = os.environ.get("FNO_PI_LIVE") == "1"
PI_ON_PATH = (
    subprocess.run(["which", "pi"], capture_output=True).returncode == 0  # noqa: S603,S607
)

_TEST_DIR = Path(__file__).resolve().parent
_REPO = _TEST_DIR.parents[2]
_CLI_SRC = _REPO / "cli" / "src"


def _crate_bin(crate: str, name: str) -> Path | None:
    """Prefer the worktree's cargo-built binary: the journey must run the code
    under test, not a deployed binary that can predate the spawn seam."""
    for profile in ("debug", "release"):
        built = _REPO / "crates" / crate / "target" / profile / name
        if built.is_file() and os.access(built, os.X_OK):
            return built
    return None


_worker_bin = _crate_bin("fno-agents", "fno-agents-worker")
_client_bin = _crate_bin("fno-agents", "fno-agents")
_front_bin = _crate_bin("fno", "fno")

_SKIP = ""
if not LIVE:
    _SKIP = "live pi journey spends subscription tokens; set FNO_PI_LIVE=1 to run"
elif not PI_ON_PATH:
    _SKIP = "pi is not on PATH"
elif _worker_bin is None:
    _SKIP = "no fno-agents-worker binary; cargo build -p fno-agents first"
elif _client_bin is None:
    _SKIP = "no fno-agents client binary; cargo build -p fno-agents first"
elif _front_bin is None:
    _SKIP = "no rust front binary; cargo build -p fno first"

_JOURNEY_NAME = "wk-x43bd"
SEED = "Reply with just: OK"
PROBE = "/target x-43bd (the footnote target verb): report what you can do, then stop."
LOOPCHECK_MARKER = ".pi-loopcheck-"


def _ps_field(field: str, pid: int) -> str:
    out = subprocess.run(  # noqa: S603
        ["ps", "-o", f"{field}=", "-p", str(pid)],
        capture_output=True,
        text=True,
    )
    return out.stdout.strip()


def _alive(pid: int) -> bool:
    return bool(_ps_field("pid", pid))


def _wait_pi_ready(sock: Path, timeout_s: float = 90.0) -> bool:
    """Read keeper Output frames until pi's own status bar has painted.

    Same read as the restart journey: pi's first frame alone proves nothing
    about readiness, and a paste into an unready TUI gets swallowed.
    """
    deadline = time.monotonic() + timeout_s
    buf = b""
    conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    conn.settimeout(2.0)
    try:
        conn.connect(str(sock))
        while time.monotonic() < deadline:
            try:
                chunk = conn.recv(4096)
            except socket.timeout:
                continue
            if not chunk:
                continue
            buf += chunk
            if b"(sub)" in buf and b"(auto)" in buf:
                time.sleep(0.5)
                return True
    finally:
        conn.close()
    return False


def _assistant_texts(cwd: Path, session_id: str) -> list[str]:
    """The assistant texts pi has recorded for one (cwd, session id) pair."""
    lookup = lookup_sessions(cwd, session_id)
    texts: list[str] = []
    for path in lookup.files:
        for line in path.read_text(encoding="utf-8").splitlines():
            try:
                row = json.loads(line)
            except ValueError:
                continue
            if row.get("type") != "message":
                continue
            message = row.get("message") or {}
            if message.get("role") != "assistant":
                continue
            if message.get("stopReason") == "error":
                raise AssertionError(
                    f"pi errored on a turn: {message.get('errorMessage')!r}"
                )
            texts.extend(
                part.get("text", "")
                for part in (message.get("content") or [])
                if isinstance(part, dict) and part.get("type") == "text"
            )
    return texts


def _wait_for_turn(cwd: Path, session_id: str, baseline: int, timeout_s: float) -> str:
    """Wait for ANY fresh assistant text - a turn landing, whatever it says."""
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        texts = _assistant_texts(cwd, session_id)
        if len(texts) > baseline:
            return texts[-1]
        time.sleep(2.0)
    texts = _assistant_texts(cwd, session_id)
    raise AssertionError(
        f"no assistant turn within {timeout_s:.0f}s; texts seen: {texts!r}"
    )


def _send_mail(text: str, session_id: str, monkeypatch, capsys) -> None:
    """One prompt through the real mail lane, with the injector binary
    resolved to the worktree build. A not-confirmed landing is recorded,
    never fatal: the receipt's confirm window is short, and the transcript
    assertions below are the gate."""
    from fno.agents.discover import DiscoveredSession
    from fno.mail import cli as mail_cli

    monkeypatch.delenv("FNO_BUS_DIR", raising=False)
    import fno.rust_binary as rust_binary_mod

    monkeypatch.setattr(rust_binary_mod, "resolve_installed_binary", lambda: _client_bin)
    resolved = DiscoveredSession(
        session_id=session_id,
        short_id="",
        handle=_JOURNEY_NAME,
        pid=0,
        cwd="",
        project=None,
        status="live",
        agent="pi",
    )
    mail_cli._name_lane_send(text, from_name="web", resolved=resolved)
    out = capsys.readouterr().out
    if "delivered (hosted)" not in out:
        print(f"mail receipt without confirm: {out.strip()}")


def _write_loopcheck_wrapper(short_state: Path) -> Path:
    """FNO_AGENTS_BIN -> a wrapper that logs the argv, then execs the REAL
    gate. The log line is the positive marker that the extension (whose
    synth transcript names .pi-loopcheck-) invoked loop-check; the exec
    keeps loop-check's own decision honest rather than staged."""
    wrapper = short_state / "loopcheck-wrapper.sh"
    log = short_state / "loopcheck.log"
    wrapper.write_text(
        "#!/bin/sh\n"
        'echo "$@" >> "$FNO_LOOPCHECK_LOG"\n'
        f'exec "{_client_bin}" "$@"\n',
        encoding="utf-8",
    )
    wrapper.chmod(0o755)
    return wrapper


def _loopcheck_log_lines(short_state: Path) -> list[str]:
    log = short_state / "loopcheck.log"
    if not log.exists():
        return []
    return [
        line
        for line in log.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]


@pytest.mark.skipif(bool(_SKIP), reason=_SKIP)
def test_AC1_HP_the_spawn_seam_journey_on_a_real_pi_thread(
    tmp_path, monkeypatch, capsys
) -> None:
    """The spawn-seam journey, end to end, on a real terminal."""
    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.setenv("FNO_INBOX_ROOT", str(tmp_path / "inbox"))
    # The real HOME, because pi's credential lives under it and the extension
    # installs into pi's own global extensions dir under it. The cwd and state
    # stay isolated, so the real home is only READ for credentials and pi's
    # own session store - plus the one install surface this journey owns,
    # restored in the finally.
    user = os.environ.get("USER", "")
    real_home = next(
        (
            candidate
            for candidate in (Path("/Users") / user, Path("/home") / user, Path("/root"))
            if candidate.is_dir()
        ),
        None,
    )
    assert real_home is not None, "the live pi journey could not locate the real HOME"
    monkeypatch.setenv("HOME", str(real_home))
    monkeypatch.setenv("USERPROFILE", str(real_home))

    # Install the shipped extension through the REAL integration arm - the
    # same code `fno config setup` runs. Snapshot first so the finally can
    # restore exactly what was there.
    from fno.setup import integration as I

    dest = I._pi_extension_dest()
    pre_installed = I._pi_is_installed()
    pre_text = dest.read_text(encoding="utf-8") if dest.exists() else None
    install_res = I._pi_install()
    assert install_res.ok, f"the pi extension install failed: {install_res.note}"
    assert I._pi_is_installed(), "the install must report installed when fresh"

    # A keeper socket must fit AF_UNIX's 104-byte sun_path and the pytest
    # basetemp does not: same short-state move as the restart journey.
    short_state = Path(tempfile.mkdtemp(prefix="fno5d-"))
    settings = tmp_path / ".fno" / "settings.yaml"
    settings.write_text(
        f"schema_version: 1\nconfig:\n  state_dir: {short_state}/\n",
        encoding="utf-8",
    )
    agents_home = short_state / "agents"
    monkeypatch.setenv("FNO_AGENTS_HOME", str(agents_home))
    monkeypatch.setenv("FNO_MUX_DIR", str(short_state / "mux"))
    monkeypatch.setenv("FNO_AGENTS_WORKER_BIN", str(_worker_bin))
    monkeypatch.setenv("FNO_AGENTS_IDLE_EXIT_SECS", "1800")
    monkeypatch.setenv("PATH", f"{_front_bin.parent}{os.pathsep}{os.environ['PATH']}")
    monkeypatch.setenv("TERM", os.environ.get("TERM") or "xterm-256color")

    # The loop gate rides FNO_AGENTS_BIN, which pi inherits through the
    # keeper: the wrapper logs the argv, then execs the real gate.
    wrapper = _write_loopcheck_wrapper(short_state)
    monkeypatch.setenv("FNO_AGENTS_BIN", str(wrapper))
    monkeypatch.setenv("FNO_LOOPCHECK_LOG", str(short_state / "loopcheck.log"))

    journey_cwd = tmp_path / "journey"
    (journey_cwd / ".fno").mkdir(parents=True)
    # The manifest is the extension's presence guard AND loop-check's state:
    # a minimal one, enough for the gate to run and answer non-terminal.
    (journey_cwd / ".fno" / "target-state.md").write_text(
        "---\n"
        "session_id: wk-x43bd-journey\n"
        'created_at: "2026-09-02T00:00:00Z"\n'
        "---\n\n"
        "# journey manifest\n",
        encoding="utf-8",
    )

    keeper_pid: int | None = None
    try:
        # ---- Step 1: spawn through the PUBLIC surface. The receipt is not
        # the proof; the registry row, the keeper Identify reply, and pi's
        # own session store are. The seed's injector binary resolves to the
        # worktree build - a deployed binary can predate the keeper lane.
        import fno.rust_binary as rust_binary_mod

        monkeypatch.setattr(rust_binary_mod, "resolve_installed_binary", lambda: _client_bin)
        from fno.agents.cli import agents_app
        from typer.testing import CliRunner

        result = CliRunner().invoke(
            agents_app,
            [
                "spawn",
                "--name", _JOURNEY_NAME,
                "--harness", "pi",
                "--substrate", "thread",
                "--cwd", str(journey_cwd),
                SEED,
            ],
        )
        assert result.exit_code == 0, (
            f"the public spawn surface refused: {result.output}"
        )
        assert "unknown harness" not in result.output
        receipt = json.loads(result.output.strip().splitlines()[-1])
        assert receipt["harness"] == "pi"
        session_id = receipt["short_id"]
        assert session_id, "the receipt carries no minted session id"

        # ---- Step 2: the registered row carries the minted id and a keeper
        # socket, and the KEEPER behind it answers Identify with the same id
        # and a live child. Positive control for the whole lane.
        rows = [e for e in load_registry() if e.name == _JOURNEY_NAME]
        assert rows, "no registry row was registered by the spawn"
        row = rows[0]
        assert row.harness_session_id == session_id
        assert row.substrate == "thread"
        assert row.messaging_socket_path, "the row carries no keeper socket"
        keeper_sock = Path(row.messaging_socket_path)
        assert row.keeper_child_pid and _alive(row.keeper_child_pid), (
            f"keeper child pid {row.keeper_child_pid} is not alive"
        )
        keeper_pid = row.pid

        from fno.agents.dispatch import _keeper_identify

        identify = _keeper_identify(keeper_sock)
        assert identify["session_id"] == session_id
        assert identify["child_pid"] == row.keeper_child_pid

        # The hosted TUI is really up before the seed's answer is awaited.
        assert _wait_pi_ready(keeper_sock), (
            "pi's status bar never painted: the hosted TUI never became ready"
        )

        # ---- Step 3: the seed landed - a fresh assistant turn answered it.
        # An unlanded seed strands a live worker with no orders, which is
        # the strand the spawn seam must not produce.
        baseline = len(_assistant_texts(journey_cwd, session_id))
        _wait_for_turn(journey_cwd, session_id, baseline, 180.0)

        # The minted id matches PI'S OWN session store - never a proxy.
        lookup = lookup_sessions(journey_cwd, session_id)
        assert lookup.files, (
            f"session id {session_id} matches no file in pi's session store"
        )

        # ---- Step 4: a /target-family message over the mail lane produces
        # a fresh turn, and the loop extension shells the REAL gate at the
        # agent_settled boundary. The log line naming a .pi-loopcheck- synth
        # transcript is the marker that the EXTENSION invoked it - no other
        # component synthesizes that shape.
        _send_mail(PROBE, session_id, monkeypatch, capsys)
        baseline = len(_assistant_texts(journey_cwd, session_id))
        _wait_for_turn(journey_cwd, session_id, baseline, 180.0)

        deadline = time.monotonic() + 60
        invocations: list[str] = []
        while time.monotonic() < deadline:
            invocations = [
                line
                for line in _loopcheck_log_lines(short_state)
                if "loop-check" in line and LOOPCHECK_MARKER in line
            ]
            if invocations:
                break
            time.sleep(1.0)
        assert invocations, (
            "the loop extension never invoked loop-check at agent_settled; "
            f"raw log: {_loopcheck_log_lines(short_state) or 'empty'}"
        )

        # ---- Cleanup, dogfooding the stop arm the restart journey proved:
        # the child goes with the keeper stop.
        from fno.agents.dispatch import stop_agent

        stopped = stop_agent(_JOURNEY_NAME)
        assert stopped.name == _JOURNEY_NAME
        child_pid = row.keeper_child_pid
        deadline = time.monotonic() + 10
        while _alive(child_pid) and time.monotonic() < deadline:
            time.sleep(0.2)
        assert not _alive(child_pid), "the child outlived a confirmed keeper stop"
    finally:
        if keeper_pid and _alive(keeper_pid):
            try:
                os.kill(keeper_pid, signal.SIGKILL)
            except OSError:
                pass
        # Restore whatever the extension dir carried before the journey.
        try:
            if pre_text is not None:
                dest.write_text(pre_text, encoding="utf-8")
            elif dest.exists():
                dest.unlink()
        except OSError:
            pass
        shutil.rmtree(short_state, ignore_errors=True)
