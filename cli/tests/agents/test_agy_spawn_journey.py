"""The agy spawn-seam journey: the PUBLIC surface reaches agy's keeper-hosted
thread lane, and agy's own stop hook shells the completion gate.

This is the bar agy's caps row set when it flipped ``thread = true``. The
journey enters through ``fno agents spawn -H agy --substrate thread`` and
proves, with positive markers only:

  1. the spawn registers a row whose ``harness_session_id`` is the minted
     conversation id the receipt carried, and whose ``messaging_socket_path``
     answers Identify with the same id and child pid;
  2. that id names a real db in AGY'S OWN conversation store - never a proxy
     read, never a bare exit code;
  3. the seed the spawn carried actually landed in the worker: the hosted TUI
     paints the token the seed asked for. A live row with no orders is the
     strand this seam must not produce;
  4. a ``/target``-family message dispatched over the mail lane produces a
     fresh painted turn, and agy's Stop hook shells ``fno-agents loop-check``
     at least once - recorded via FNO_AGENTS_BIN pointing at a wrapper that
     logs the argv, then execs the real gate. The gate's DECISION is not
     asserted: loop-check is the sole completion authority, and what it
     decides about this synthetic manifest is its own business.

Assertion 3 and 4 read the KEEPER'S OUTPUT STREAM rather than a transcript
file, because agy keeps no local per-turn transcript store that fno can scan -
its conversations live in a sqlite db whose schema fno does not read. The
paint IS the turn: the token appears only because the model produced it.

The live test is opt-in (``FNO_AGY_LIVE=1``) because it spends real
subscription tokens and needs this machine's agy credentials. It installs the
shipped Stop adapter into the real ``~/.gemini/config/hooks.json`` (the
surface agy actually reads, and exactly what ``fno config setup`` does) and
restores whatever was there before.

RUN OF RECORD, 2026-09-03, agy 1.1.24 on a live Google AI Pro subscription,
worktree debug builds: **GREEN on the post-review arm, 30.8s** (evidence
``child_pid=73588 keeper_pid=73587
session_id=a77c7c9c-9033-49cc-b95f-d15f505ae755``), after the same green on the
pre-review arm at 213.4s (evidence ``child_pid=72851 keeper_pid=72849
session_id=71cc2d46-9c01-4fb4-8266-f7cf79fe227c``). Re-run because round 1
changed the modal-answer path this journey exercises: an attestation from
before a fix is an attestation for other code. The spread is agy's own latency,
not the lane's - both runs made the same four assertions in the same order, and
the evidence line prints only after the last one.

Four things had to be built before it could pass, and each was a real gap the
journey found rather than a test bug:

  - the keeper read the session id off ``--session-id`` and ``--resume`` only,
    so agy's ``--conversation <uuid>`` answered Identify with ``None``;
  - the trust-file upsert alone did not clear agy's folder-trust modal, and a
    keeper has nobody to answer one, so the seed submit now answers it;
  - the keeper mail lane had no confirm source for agy and demoted every
    probe to ``no-confirm-source``;
  - ``FNO_AGY_LIVE`` was swept by the hermetic env, so this journey skipped
    for anyone who set it - the exact trap that list's comment already names
    for pi.
"""
from __future__ import annotations

import json
import os
import re
import shutil
import signal
import socket
import subprocess
import tempfile
import time
from pathlib import Path

import pytest

from fno.agents.dispatch import DispatchAskError
from fno.agents.harnesses.agy import conversation_store_path
from fno.agents.registry import load_registry
from fno.paths_testing import use_tmpdir

LIVE = os.environ.get("FNO_AGY_LIVE") == "1"
AGY_ON_PATH = (
    subprocess.run(["which", "agy"], capture_output=True).returncode == 0  # noqa: S603,S607
)

_TEST_DIR = Path(__file__).resolve().parent
_REPO = _TEST_DIR.parents[2]


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
    _SKIP = "live agy journey spends subscription tokens; set FNO_AGY_LIVE=1 to run"
elif not AGY_ON_PATH:
    _SKIP = "agy is not on PATH"
elif _worker_bin is None:
    _SKIP = "no fno-agents-worker binary; cargo build -p fno-agents first"
elif _client_bin is None:
    _SKIP = "no fno-agents client binary; cargo build -p fno-agents first"
elif _front_bin is None:
    _SKIP = "no rust front binary; cargo build -p fno first"

_JOURNEY_NAME = "wk-x-d145"
SEED_TOKEN = "AGYSEED-4417"
SEED = f"Reply with exactly: {SEED_TOKEN}"
# The token the model must PRODUCE. The probe never spells it, because a pasted
# envelope repaints into the same screen the answer does: a marker the prompt
# carries would confirm delivery and be read as a turn.
PROBE_TOKEN = "YTIVARGITNA"
PROBE = (
    "/target (the footnote target verb): say in one line what you can do, and "
    "end your reply with the word ANTIGRAVITY spelled backwards, in capitals."
)
# The `--transcript` path agy's Stop adapter synthesizes. Nothing else in the
# fleet writes this shape, so a loop-check argv naming it proves the ADAPTER
# invoked the gate rather than some other caller.
LOOPCHECK_MARKER = ".agy-loopcheck-"
# agy's composer-idle paint, the same marker the spawn arm seeds against.
COMPOSER_MARKER = b"? for shortcuts"

_ANSI = re.compile(rb"\x1b\[[0-9;?]*[A-Za-z]|\x1b\][^\x07]*\x07|\x1b[=>][0-9;]*")


def _ps_field(field: str, pid: int) -> str:
    out = subprocess.run(  # noqa: S603
        ["ps", "-o", f"{field}=", "-p", str(pid)],
        capture_output=True,
        text=True,
    )
    return out.stdout.strip()


def _alive(pid: int) -> bool:
    return bool(_ps_field("pid", pid))


def _read_paint(sock: Path, marker: bytes, timeout_s: float) -> bool:
    """Read keeper Output frames until ``marker`` paints, nudging a repaint.

    The keeper never replays its ring to a late connection and a same-size
    Resize fires no SIGWINCH, so the sizes alternate to force a full repaint
    of whatever is already on screen. Same dance the seed submit runs.
    """
    def frame(tag: int, payload: bytes) -> bytes:
        return bytes([tag]) + len(payload).to_bytes(4, "little") + payload

    raw = bytearray()
    text = bytearray()
    conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    conn.settimeout(2.0)
    try:
        conn.connect(str(sock))
        deadline = time.monotonic() + timeout_s
        flip = False
        last_nudge = 0.0
        while time.monotonic() < deadline:
            if marker in bytes(text):
                return True
            now = time.monotonic()
            if now - last_nudge >= 4.0:
                flip = not flip
                rows, cols = (45, 161) if flip else (44, 160)
                conn.sendall(
                    frame(
                        2,
                        rows.to_bytes(2, "little") + cols.to_bytes(2, "little"),
                    )
                )
                last_nudge = now
            try:
                chunk = conn.recv(65536)
            except socket.timeout:
                continue
            if not chunk:
                time.sleep(0.2)
                continue
            raw.extend(chunk)
            decoded = bytearray()
            while len(raw) >= 5:
                length = int.from_bytes(raw[1:5], "little")
                if length > 1_048_576 or len(raw) < 5 + length:
                    break
                decoded.extend(raw[5 : 5 + length])
                del raw[: 5 + length]
            text.extend(_ANSI.sub(b"", bytes(decoded)))
        return marker in bytes(text)
    finally:
        conn.close()


def _send_mail(text: str, session_id: str, monkeypatch, capsys) -> None:
    """One prompt through the real mail lane, with the injector binary
    resolved to the worktree build. A not-confirmed landing is recorded,
    never fatal: the receipt's confirm window is short, and the paint
    assertion below is the gate."""
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
        agent="agy",
    )
    mail_cli._name_lane_send(text, from_name="web", resolved=resolved)
    out = capsys.readouterr().out
    if "delivered (hosted)" not in out:
        print(f"mail receipt without confirm: {out.strip()}")


def _write_loopcheck_wrapper(short_state: Path) -> Path:
    """FNO_AGENTS_BIN -> a wrapper that logs the argv, then execs the REAL
    gate. The log line is the positive marker that agy's Stop adapter (whose
    synth transcript names .agy-loopcheck-) invoked loop-check; the exec keeps
    loop-check's own decision honest rather than staged."""
    wrapper = short_state / "loopcheck-wrapper.sh"
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
    return [line for line in log.read_text(encoding="utf-8").splitlines() if line.strip()]


@pytest.mark.skipif(bool(_SKIP), reason=_SKIP)
def test_AC1_HP_the_spawn_seam_journey_on_a_real_agy_thread(
    tmp_path, monkeypatch, capsys
) -> None:
    """The spawn-seam journey, end to end, on a real terminal."""
    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.setenv("FNO_INBOX_ROOT", str(tmp_path / "inbox"))
    # The real HOME, because agy's credential, its conversation store, and the
    # hooks.json the adapter installs into all live under it. The cwd and state
    # stay isolated, so the real home is only READ for credentials and the
    # store - plus the one install surface this journey owns, restored in the
    # finally.
    user = os.environ.get("USER", "")
    real_home = next(
        (
            candidate
            for candidate in (Path("/Users") / user, Path("/home") / user, Path("/root"))
            if candidate.is_dir()
        ),
        None,
    )
    assert real_home is not None, "the live agy journey could not locate the real HOME"
    monkeypatch.setenv("HOME", str(real_home))
    monkeypatch.setenv("USERPROFILE", str(real_home))

    # Install the shipped Stop adapter through the REAL integration arm - the
    # same code `fno config setup` runs. Snapshot first so the finally can
    # restore exactly what was there.
    from fno.setup import integration as I

    hooks_json = I._agy_hooks_json()
    pre_hooks = hooks_json.read_text(encoding="utf-8") if hooks_json.exists() else None
    install_res = I._agy_install()
    assert install_res.ok, f"the agy Stop adapter install failed: {install_res.note}"

    # A keeper socket must fit AF_UNIX's 104-byte sun_path and the pytest
    # basetemp does not: the same short-state move the pi journey makes.
    short_state = Path(tempfile.mkdtemp(prefix="fnoagy-"))
    settings = tmp_path / ".fno" / "settings.yaml"
    settings.write_text(
        f"schema_version: 1\nconfig:\n  state_dir: {short_state}/\n",
        encoding="utf-8",
    )
    monkeypatch.setenv("FNO_AGENTS_HOME", str(short_state / "agents"))
    monkeypatch.setenv("FNO_MUX_DIR", str(short_state / "mux"))
    monkeypatch.setenv("FNO_AGENTS_WORKER_BIN", str(_worker_bin))
    monkeypatch.setenv("FNO_AGENTS_IDLE_EXIT_SECS", "1800")
    monkeypatch.setenv("PATH", f"{_front_bin.parent}{os.pathsep}{os.environ['PATH']}")
    monkeypatch.setenv("TERM", os.environ.get("TERM") or "xterm-256color")

    # The loop gate rides FNO_AGENTS_BIN, which agy inherits through the
    # keeper: the wrapper logs the argv, then execs the real gate.
    wrapper = _write_loopcheck_wrapper(short_state)
    monkeypatch.setenv("FNO_AGENTS_BIN", str(wrapper))
    monkeypatch.setenv("FNO_LOOPCHECK_LOG", str(short_state / "loopcheck.log"))

    journey_cwd = tmp_path / "journey"
    (journey_cwd / ".fno").mkdir(parents=True)

    keeper_pid: int | None = None
    child_pid_holder: list[int] = []
    try:
        # ---- Step 1: spawn through the PUBLIC surface. The receipt is not the
        # proof; the registry row, the keeper Identify reply, and agy's own
        # conversation store are. The seed's injector binary resolves to the
        # worktree build - a deployed binary can predate the keeper lane.
        import fno.rust_binary as rust_binary_mod

        monkeypatch.setattr(rust_binary_mod, "resolve_installed_binary", lambda: _client_bin)
        from typer.testing import CliRunner

        from fno.agents.cli import agents_app

        result = CliRunner().invoke(
            agents_app,
            [
                "spawn",
                "--name", _JOURNEY_NAME,
                "--harness", "agy",
                "--substrate", "thread",
                "--cwd", str(journey_cwd),
                SEED,
            ],
        )
        assert result.exit_code == 0, (
            f"the public spawn surface refused: {result.output}"
        )
        assert "unknown harness" not in result.output
        assert "no measured thread lane" not in result.output
        receipt = json.loads(result.output.strip().splitlines()[-1])
        assert receipt["harness"] == "agy"
        session_id = receipt["short_id"]
        assert session_id, "the receipt carries no minted conversation id"

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
        child_pid_holder.append(row.keeper_child_pid)

        from fno.agents.dispatch import _keeper_identify

        identify = _keeper_identify(keeper_sock)
        assert identify["session_id"] == session_id
        assert identify["child_pid"] == row.keeper_child_pid

        # The minted id names a real conversation in AGY'S OWN store. fno never
        # writes this file, so its presence is the harness agreeing that the id
        # is one of its conversations.
        store = conversation_store_path(session_id)
        assert store.is_file(), (
            f"minted id {session_id} names no db in agy's conversation store "
            f"({store})"
        )

        # The manifest is the Stop adapter's active-session discriminator, and
        # it is keyed on agy's OWN conversation id - which only exists after
        # the spawn, so it is written here rather than before.
        # `harness_session_id` is the key manifest-for-session matches on; a
        # `session_id` line is invisible to it and the adapter answers "no
        # manifest names session <id>" while looking like it never ran.
        (journey_cwd / ".fno" / "target-state.md").write_text(
            "---\n"
            "harness: agy\n"
            f"harness_session_id: {session_id}\n"
            f"owner_cwd: {journey_cwd}\n"
            'created_at: "2026-09-03T00:00:00Z"\n'
            "---\n\n"
            "# journey manifest\n",
            encoding="utf-8",
        )

        # ---- Step 3: the seed landed - the hosted TUI paints the token the
        # seed asked for. An unlanded seed strands a live worker with no
        # orders, which is the strand the spawn seam must not produce.
        assert _read_paint(keeper_sock, COMPOSER_MARKER, 120.0), (
            "agy's composer never painted: the hosted TUI never became ready"
        )
        assert _read_paint(keeper_sock, SEED_TOKEN.encode(), 180.0), (
            f"the seed never produced {SEED_TOKEN} in the hosted TUI"
        )

        # ---- Step 4: a /target-family message over the mail lane produces a
        # fresh painted turn, and the Stop adapter shells the REAL gate. The
        # log line naming a .agy-loopcheck- synth transcript is the marker that
        # the ADAPTER invoked it - no other component synthesizes that shape.
        _send_mail(PROBE, session_id, monkeypatch, capsys)
        # The token the MODEL must produce, never a word the probe itself
        # carries: a pasted envelope echoes into the same paint, so matching
        # the prompt would confirm delivery and call it a turn.
        assert _read_paint(keeper_sock, PROBE_TOKEN.encode(), 300.0), (
            "the mailed probe never produced a fresh assistant turn"
        )

        deadline = time.monotonic() + 120
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
            "agy's Stop adapter never invoked loop-check; "
            f"raw log: {_loopcheck_log_lines(short_state) or 'empty'}"
        )

        # ---- Cleanup, dogfooding the stop arm: the child goes with the keeper
        # stop. The evidence line prints the named pid and session id a run of
        # record must carry - a pass with no named pid is not a pass.
        print(
            f"RUN-OF-RECORD agy spawn-seam journey: "
            f"child_pid={row.keeper_child_pid} "
            f"keeper_pid={row.pid} session_id={session_id}"
        )
        from fno.agents.dispatch import stop_agent

        child_pid = row.keeper_child_pid
        # A stop refusal under load is not a strand: agy's own exit can outlast
        # the stop grace window while the Kill contract still completes. The
        # gate is the child's DEATH, verified, never the verb's receipt.
        try:
            stopped = stop_agent(_JOURNEY_NAME)
            assert stopped.name == _JOURNEY_NAME
        except DispatchAskError as stop_refusal:
            print(f"stop refused (grace window), verifying death directly: {stop_refusal}")
        deadline = time.monotonic() + 15
        while _alive(child_pid) and time.monotonic() < deadline:
            time.sleep(0.2)
        assert not _alive(child_pid), "the child outlived the keeper stop"
    finally:
        if keeper_pid and _alive(keeper_pid):
            try:
                os.kill(keeper_pid, signal.SIGKILL)
            except OSError:
                pass
        if child_pid_holder and child_pid_holder[0] and _alive(child_pid_holder[0]):
            # Last resort only: the keeper is dead and the child survived it.
            # A stranded agy TUI outlives the journey, so it dies here, named.
            print(f"SIGKILL surviving child {child_pid_holder[0]}")
            try:
                os.kill(child_pid_holder[0], signal.SIGKILL)
            except OSError:
                pass
        # Restore whatever hooks.json carried before the journey.
        try:
            if pre_hooks is not None:
                hooks_json.write_text(pre_hooks, encoding="utf-8")
            elif hooks_json.exists():
                hooks_json.unlink()
        except OSError:
            pass
        shutil.rmtree(short_state, ignore_errors=True)


def test_the_journey_skip_reason_is_named_when_it_does_not_run() -> None:
    """AC4-EDGE: with FNO_AGY_LIVE unset or agy absent, the journey SKIPS with
    the reason named rather than passing vacuously. A green suite that never
    ran the lane must say so."""
    if LIVE and AGY_ON_PATH and _worker_bin and _client_bin and _front_bin:
        assert _SKIP == "", "a fully live environment must not skip"
    else:
        assert _SKIP, "a non-live environment must name why the journey is skipped"
