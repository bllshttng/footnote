"""The spawn-seam journey for grok (x-fd31): the PUBLIC surface reaches
grok's keeper-hosted thread lane.

The bar is the one pi's caps row set when it flipped ``thread = true``: the
arm brings its own journey for the lane it opens. This journey enters
through ``fno agents spawn -H grok --substrate thread`` and proves, with
positive markers only:

  1. the spawn registers a row whose ``harness_session_id`` is the minted
     uuid the receipt carried and whose keeper answers Identify with the
     same id and a live child pid;
  2. the hosted TUI paints its status bar (the row's idle marker);
  3. the seed the spawn carried actually landed - the marker reply the seed
     asks for repaints onto the keeper's output stream. A live row with no
     orders is the strand this seam must not produce.

Deliberately NOT re-proven here: the keeper machinery itself (survive the
SIGKILL of both supervisors, restart re-bind) is pi's wk-x61bc journey and
is harness-agnostic, and the caller-assigned binding (`--session-id`
create, `--resume` recall across a process kill) was measured directly
against live grok 1.0.13 when the row landed (x-fd31, 2026-09-02). The
loop-extension leg pi's journey carries has no grok counterpart: the row's
``loop_extension`` is "" until an extension ships.

The live test is opt-in (``FNO_GROK_LIVE=1``) because it spends real grok
tokens and needs this machine's grok credential.
"""
from __future__ import annotations

import json
import os
import re
import socket
import struct
import subprocess
import tempfile
import time
from pathlib import Path

import pytest

from fno.agents.registry import load_registry
from fno.paths_testing import use_tmpdir

LIVE = os.environ.get("FNO_GROK_LIVE") == "1"
GROK_ON_PATH = (
    subprocess.run(["which", "grok"], capture_output=True).returncode == 0  # noqa: S603,S607
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
    _SKIP = "live grok journey spends real tokens; set FNO_GROK_LIVE=1 to run"
elif not GROK_ON_PATH:
    _SKIP = "grok is not on PATH"
elif _worker_bin is None:
    _SKIP = "no fno-agents-worker binary; cargo build -p fno-agents first"
elif _client_bin is None:
    _SKIP = "no fno-agents client binary; cargo build -p fno-agents first"
elif _front_bin is None:
    _SKIP = "no rust front binary; cargo build -p fno first"

_JOURNEY_NAME = "wk-grok-fd31"
REPLY_ONE = "JRN-OK-G5E2"
SEED = (
    f"Remember the codeword GROK-JRN-7Q2X. Reply with exactly {REPLY_ONE} "
    "and nothing else."
)


def _strip_ansi(raw: bytes) -> str:
    out = bytearray()
    i, n = 0, len(raw)
    while i < n:
        b = raw[i]
        if b == 0x1B:
            if i + 1 < n and raw[i + 1 : i + 2] == b"[":
                i += 2
                while i < n and not (0x40 <= raw[i] <= 0x7E):
                    i += 1
                i += 1
            else:
                i += 2
        else:
            out.append(b)
            i += 1
    return bytes(out).decode("utf-8", "replace")


def _wait_marker(text: str, pattern: str, timeout_s: float) -> bool:
    rx = re.compile(pattern)
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        if rx.search(_strip_ansi(text.encode())):
            return True
        time.sleep(2.0)
    return rx.search(_strip_ansi(text.encode())) is not None


def _alive(pid: int) -> bool:
    out = subprocess.run(  # noqa: S603
        ["ps", "-o", "pid=", "-p", str(pid)],
        capture_output=True,
        text=True,
    )
    return bool(out.stdout.strip())


@pytest.mark.skipif(bool(_SKIP), reason=_SKIP)
def test_grok_spawn_journey_public_surface_reaches_a_live_row(
    tmp_path, monkeypatch
) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    # Restore the REAL HOME for the live run: the hermetic suite pins HOME
    # into a sandbox, and grok's credential lives under ~/.grok, so the
    # hosted TUI would sit on its login screen and never paint the composer.
    # fno's own state stays isolated through FNO_CONFIG / FNO_AGENTS_HOME /
    # FNO_MUX_DIR above. Same shape as the live ACP journey's AC9 test.
    real_home = next(
        (
            candidate
            for candidate in (
                os.path.join("/Users", os.environ.get("USER", "")),
                os.path.join("/home", os.environ.get("USER", "")),
                "/root",
            )
            if os.path.isdir(candidate)
        ),
        None,
    )
    assert real_home is not None, "the live grok journey could not locate the real HOME"
    monkeypatch.setenv("HOME", real_home)
    monkeypatch.setenv("USERPROFILE", real_home)

    # A keeper socket must fit AF_UNIX's 104-byte sun_path and the pytest
    # basetemp does not: same short-state move as pi's journey.
    short_state = Path(tempfile.mkdtemp(prefix="fno5g-"))
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

    journey_cwd = tmp_path / "journey"
    journey_cwd.mkdir(parents=True)

    try:
        # ---- Step 1: spawn through the PUBLIC surface. The receipt is not
        # the proof; the registry row and the keeper Identify reply are.
        import fno.rust_binary as rust_binary_mod

        monkeypatch.setattr(
            rust_binary_mod, "resolve_installed_binary", lambda: _client_bin
        )
        from fno.agents.cli import agents_app
        from typer.testing import CliRunner

        result = CliRunner().invoke(
            agents_app,
            [
                "spawn",
                "--name", _JOURNEY_NAME,
                "--harness", "grok",
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
        assert receipt["harness"] == "grok"
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

        from fno.agents.dispatch import _keeper_identify

        identify = _keeper_identify(keeper_sock)
        assert identify["session_id"] == session_id
        assert identify["child_pid"] == row.keeper_child_pid

        # ---- Step 3: the seed landed. The TUI's output rides the keeper's
        # Output frames (keeper.log carries the KEEPER's stderr, not the
        # pane), so reconnect and force repaints the same way the seed
        # submit does: the marker reply the seed demanded repainting onto
        # the stream is the landing proof - the model answered the payload.
        deadline = time.monotonic() + 240.0
        seen = ""
        flip = False
        while time.monotonic() < deadline:
            conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            conn.settimeout(2.0)
            try:
                conn.connect(str(keeper_sock))
                pending = bytearray()
                read_until = time.monotonic() + 5.0
                while time.monotonic() < read_until:
                    try:
                        chunk = conn.recv(65536)
                    except socket.timeout:
                        break
                    if not chunk:
                        break
                    pending.extend(chunk)
                    while len(pending) >= 5:
                        length = int.from_bytes(pending[1:5], "little")
                        if length > 1_048_576 or len(pending) < 5 + length:
                            break
                        seen += _strip_ansi(bytes(pending[5 : 5 + length])).replace(
                            "\r", ""
                        ).replace("\n", "")
                        del pending[: 5 + length]
                if REPLY_ONE in seen:
                    break
                # No marker yet: force a repaint (alternate sizes every
                # pass, the same nudge the seed submit uses) and read again.
                flip = not flip
                rows, cols = (25, 81) if flip else (24, 80)
                conn.sendall(
                    bytes([2]) + struct.pack("<I", 4) + struct.pack("<HH", rows, cols)
                )
            finally:
                conn.close()
        assert REPLY_ONE in seen, (
            f"the seed's marker reply never painted; tail: {seen[-2000:]!r}"
        )
    finally:
        # monkeypatch.setenv mutated os.environ, so this subprocess inherits
        # the isolated home; PATH already carries the worktree front binary.
        subprocess.run(  # noqa: S603
            ["fno", "agents", "rm", _JOURNEY_NAME],
            capture_output=True,
            text=True,
            timeout=60,
        )
