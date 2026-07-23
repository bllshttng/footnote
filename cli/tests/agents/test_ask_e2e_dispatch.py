"""Wave A2 (ab-0429c6e1): end-to-end dispatch differential for `fno agents ask`.

Drives a `codex` ask through BOTH the Python dispatch path
(``fno.agents.dispatch.dispatch_ask``, exercised in-process) and the
Rust client (``fno-agents`` subprocess) against ONE shared fake codex
binary on PATH, then asserts identical observable behavior: stdout reply
text and exit code. With the Wave A1 provider-conditional routing flip,
the default ``auto`` runtime now execs the Rust client for codex; this
test pins the post-flip user-visible parity end-to-end.

The byte-parity contract at the library boundary is already pinned by
``crates/fno-agents/tests/codex_ask_parity.rs`` (Wave B3, this node) and
``crates/fno-agents/tests/claude_ask_parity.rs`` (parent ab-cc926b4e). The
routing decision is unit-tested in ``test_rust_runtime.py``. This file is
the extra layer on top: it confirms the two paths reach the same fake
provider with the same arguments and produce the same user-visible bytes.

Skips (not fails) when the compiled ``fno-agents`` binary is absent (sdist
test env), mirroring ``test_rust_verb_parity.py``.

Why codex but not claude in this file: codex `ask` create is a one-shot
subprocess that emits its session id and reply on stdout, fully covered
by the existing fake-codex JSONL contract. Claude `ask` create is a
``claude --bg`` shellout that backgrounds itself, returns a short-id, and
relies on a rendezvous AF_UNIX socket for the supervisor; faithfully
faking the supervisor half makes a CLI-level subprocess test fragile.
The library-level claude byte parity is already proven by
``claude_ask_parity.rs``, so we accept the asymmetry rather than ship a
flaky claude e2e.
"""
from __future__ import annotations

import os
import stat
import subprocess
from pathlib import Path

import pytest

from fno.paths_testing import use_tmpdir


# --------------------------------------------------------------------------- #
# Rust binary discovery (mirrors test_rust_verb_parity._find_rust_bin).
# --------------------------------------------------------------------------- #

def _find_rust_bin() -> Path | None:
    """Locate the compiled ``fno-agents`` client (release preferred, then debug)."""
    start = Path(__file__).resolve().parent
    for parent in [start, *start.parents]:
        crate = parent / "crates" / "fno-agents"
        if crate.is_dir():
            for profile in ("release", "debug"):
                cand = crate / "target" / profile / "fno-agents"
                if cand.is_file():
                    return cand
            return None
    return None


RUST_BIN = _find_rust_bin()
requires_rust = pytest.mark.skipif(
    RUST_BIN is None,
    reason="compiled fno-agents binary not present (build with `cargo build -p fno-agents`)",
)


# --------------------------------------------------------------------------- #
# Shared fake codex installer. The script is the exact contract emitted by
# the fake codex used in crates/fno-agents/tests/codex_ask_dispatch.rs and
# codex_ask_parity.rs (Wave B3) — keeping the wire format identical is
# what lets a single fake serve both paths.
# --------------------------------------------------------------------------- #

_FAKE_CODEX_SCRIPT = r"""#!/bin/sh
set -e
if [ -n "$FAKE_CODEX_EXIT" ] && [ "$FAKE_CODEX_EXIT" != "0" ]; then
  exit "$FAKE_CODEX_EXIT"
fi
if [ -n "$FAKE_CODEX_SESSION_ID" ]; then
  printf '{"type":"thread.started","thread_id":"%s"}\n' "$FAKE_CODEX_SESSION_ID"
fi
if [ -n "$FAKE_CODEX_REPLY" ]; then
  printf '{"type":"item.completed","item":{"type":"agent_message","text":"%s"}}\n' "$FAKE_CODEX_REPLY"
fi
printf '{"type":"turn.completed"}\n'
exit 0
"""


def _install_fake_codex(bin_dir: Path) -> None:
    bin_dir.mkdir(parents=True, exist_ok=True)
    path = bin_dir / "codex"
    path.write_text(_FAKE_CODEX_SCRIPT, encoding="utf-8")
    mode = path.stat().st_mode
    path.chmod(mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


# Per-provider fixture: (installer, env vars to set FAKE_*_SESSION_ID / _REPLY).
# Keyed so the followup matrix below stays a single parametrized test.
_PROVIDER_FAKES = {
    "codex": (
        _install_fake_codex,
        lambda sid, reply: {"FAKE_CODEX_SESSION_ID": sid, "FAKE_CODEX_REPLY": reply},
    ),
    "gemini": (
        _install_fake_gemini,
        lambda sid, reply: {"FAKE_GEMINI_SESSION_ID": sid, "FAKE_GEMINI_REPLY": reply},
    ),
}


# --------------------------------------------------------------------------- #
# Codex create differential
# --------------------------------------------------------------------------- #

# A deterministic session id + reply the fake codex echoes back. The codex
# `create` path derives the agent's short-id from the session id (8 hex chars
# of the leading uuid block), so a fixed session id makes the assertion
# deterministic for both sides.
_SESSION_ID = "11111111-2222-3333-4444-555555555555"
_REPLY = "hello from fake codex"


@requires_rust
@pytest.mark.parametrize("provider", ["codex", "gemini"])
def test_spawn_once_python_vs_rust_parity(provider, tmp_path: Path, monkeypatch) -> None:
    """A `spawn --once` (the de-overloaded home of the old ask-create
    exchange, Task 1.3) produces the same stdout + exit code regardless of
    whether the dispatch runs through Python (`dispatch_spawn`) or the Rust
    client (`fno-agents spawn --provider <p> --once`), for codex AND gemini
    (sigma-review gap Q2: gemini previously had no one-shot parity pin)."""
    install_fake, env_for = _PROVIDER_FAKES[provider]
    bin_dir = tmp_path / "bin"
    install_fake(bin_dir)

    # --- Python path: dispatch_spawn in-process against the fake provider.
    # use_tmpdir isolates FNO_HOME (registry + events.jsonl) so the
    # in-process run doesn't touch the test runner's real state.
    py_home = tmp_path / "py-home"
    py_home.mkdir()
    use_tmpdir(monkeypatch, py_home)

    # Put the fake first on PATH; preserve the rest so subprocess machinery
    # (sh, /usr/bin tools used by the pgrp setup) still resolves.
    fake_path = f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}"
    monkeypatch.setenv("PATH", fake_path)
    for k, v in env_for(_SESSION_ID, _REPLY).items():
        monkeypatch.setenv(k, v)

    from fno.agents.dispatch import dispatch_spawn

    cwd = tmp_path / "workdir"
    cwd.mkdir()
    py_result = dispatch_spawn(
        name="parity-agent",
        message="hi",
        provider=provider,
        cwd=cwd,
        once=True,
        timeout=10,
    )

    # cmd_spawn renders the once outcome as the reply verbatim on stdout
    # (no trailing newline; the teardown receipt goes to stderr).
    py_stdout = py_result.reply or ""
    py_exit = 0

    # --- Rust path: subprocess the fno-agents binary against the same fake
    # provider but a SEPARATE FNO_AGENTS_HOME so the registry write of the
    # Python run doesn't conflict with the Rust run's create.
    rs_home = tmp_path / "rs-home"
    rs_home.mkdir()
    env = {
        **os.environ,
        "FNO_AGENTS_HOME": str(rs_home),
        "PATH": fake_path,
        **env_for(_SESSION_ID, _REPLY),
    }
    completed = subprocess.run(
        [str(RUST_BIN), "spawn", "parity-agent", "hi", "--provider", provider, "--once"],
        env=env,
        capture_output=True,
        text=True,
        timeout=30,
    )
    rs_stdout = completed.stdout
    rs_exit = completed.returncode

    assert py_exit == rs_exit, (
        f"exit-code drift: python={py_exit} rust={rs_exit}\n"
        f"py stdout: {py_stdout!r}\n"
        f"rust stdout: {rs_stdout!r}\n"
        f"rust stderr: {completed.stderr}"
    )
    assert py_stdout == rs_stdout, (
        f"stdout drift (user-visible bytes):\n"
        f"  python: {py_stdout!r}\n"
        f"  rust:   {rs_stdout!r}\n"
        f"  rust stderr: {completed.stderr}"
    )
    # The once receipt is visible on the Rust stderr (Python prints its own
    # in-process; pinned by test_dispatch_spawn.py).
    assert "torn down" in completed.stderr, (
        f"rust spawn --once must print the teardown receipt on stderr: {completed.stderr!r}"
    )


@requires_rust
def test_ask_unknown_agent_python_vs_rust_parity(tmp_path: Path, monkeypatch) -> None:
    """`ask` for an unregistered name errors identically on both runtimes
    (US1 / AC1-ERR, Task 1.3): same stderr bytes, same exit 16, no create."""
    py_home = tmp_path / "py-home"
    py_home.mkdir()
    use_tmpdir(monkeypatch, py_home)

    from fno.agents.dispatch import (
        UNKNOWN_AGENT_EXIT_CODE,
        DispatchAskError,
        dispatch_ask,
    )

    cwd = tmp_path / "workdir"
    cwd.mkdir()
    with pytest.raises(DispatchAskError) as excinfo:
        dispatch_ask(name="ghost-agent", message="hi", provider="codex", cwd=cwd, timeout=10)
    assert excinfo.value.exit_code == UNKNOWN_AGENT_EXIT_CODE

    rs_home = tmp_path / "rs-home"
    rs_home.mkdir()
    completed = subprocess.run(
        [str(RUST_BIN), "ask", "ghost-agent", "hi", "--provider", "codex"],
        env={**os.environ, "FNO_AGENTS_HOME": str(rs_home)},
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert completed.returncode == UNKNOWN_AGENT_EXIT_CODE, (
        f"rust unknown-agent ask must exit {UNKNOWN_AGENT_EXIT_CODE}: "
        f"{completed.returncode} (stderr: {completed.stderr!r})"
    )
    # cmd_ask prints str(exc) + newline; the Rust client uses eprintln.
    assert completed.stderr == f"{excinfo.value}\n", (
        f"unknown-agent stderr drift:\n"
        f"  python: {str(excinfo.value)!r}\n"
        f"  rust:   {completed.stderr!r}"
    )
    assert completed.stdout == ""


@requires_rust
def test_codex_ask_short_flags_match_long_through_rust_binary(
    tmp_path: Path, monkeypatch
) -> None:
    """ab-3ff64151 AC1 (the design's highest-risk mitigation): the phone shorts
    ``-p``/``-c``/``-t`` (and Task 1.3's ``-o``) must reach dispatch on the REAL
    Rust path - the compiled ``fno-agents`` binary, not just the ``build_request``
    parse layer. Run the binary once with short flags and once with the long forms
    (each with its own FNO_AGENTS_HOME so the create-time registry writes don't
    collide) and assert byte-identical stdout + exit code. Since Task 1.3 the
    create exchange lives on ``spawn --once`` (ask never creates), so that is the
    verb under test; the in-crate unit test covers the parse arm, this covers the
    surrounding argv plumbing + dispatch."""
    bin_dir = tmp_path / "bin"
    _install_fake_codex(bin_dir)
    fake_path = f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}"
    cwd = tmp_path / "workdir"
    cwd.mkdir()

    def _run(flags: list[str], home: Path) -> subprocess.CompletedProcess[str]:
        home.mkdir()
        env = {
            **os.environ,
            "FNO_AGENTS_HOME": str(home),
            "PATH": fake_path,
            "FAKE_CODEX_SESSION_ID": _SESSION_ID,
            "FAKE_CODEX_REPLY": _REPLY,
        }
        return subprocess.run(
            # Task 1.3: ask never creates, so the create exchange the shorts
            # must reach now lives on `spawn --once` (-o is the --once short).
            [str(RUST_BIN), "spawn", "parity-agent", "hi", *flags],
            env=env,
            capture_output=True,
            text=True,
            timeout=30,
        )

    short = _run(["-p", "codex", "-c", str(cwd), "-t", "10", "-o"], tmp_path / "short-home")
    long = _run(
        ["--provider", "codex", "--cwd", str(cwd), "--timeout", "10", "--once"],
        tmp_path / "long-home",
    )

    assert short.returncode == long.returncode, (
        f"exit-code drift: short={short.returncode} long={long.returncode}\n"
        f"short stderr: {short.stderr}\nlong stderr: {long.stderr}"
    )
    assert short.returncode == 0, f"short-flag spawn --once failed: {short.stderr}"
    assert short.stdout == long.stdout, (
        "short flags must produce identical stdout to long flags through the "
        f"real binary:\n  short: {short.stdout!r}\n  long:  {long.stdout!r}\n"
        f"  short stderr: {short.stderr}\n  long stderr: {long.stderr}"
    )


@requires_rust
def test_codex_spawn_once_fake_provider_exit_propagates(tmp_path: Path, monkeypatch) -> None:
    """Both paths surface the fake codex's non-zero exit as a non-zero
    dispatch exit. Confirms the failure framing is shared, not just the
    happy path. (Task 1.3: the create exchange lives on spawn --once now;
    ask on an unknown name would exit 16 without ever running codex, which
    is the wrong subject for this test.)"""
    bin_dir = tmp_path / "bin"
    _install_fake_codex(bin_dir)

    py_home = tmp_path / "py-home"
    py_home.mkdir()
    use_tmpdir(monkeypatch, py_home)

    fake_path = f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}"
    monkeypatch.setenv("PATH", fake_path)
    # Fake codex exits 1 and produces no JSONL — the dispatcher should turn
    # that into a non-zero exit code with no reply on both sides.
    monkeypatch.setenv("FAKE_CODEX_EXIT", "1")
    monkeypatch.delenv("FAKE_CODEX_SESSION_ID", raising=False)
    monkeypatch.delenv("FAKE_CODEX_REPLY", raising=False)

    from fno.agents.dispatch import DispatchAskError, dispatch_spawn

    cwd = tmp_path / "workdir"
    cwd.mkdir()
    py_exit: int
    try:
        dispatch_spawn(
            name="parity-agent",
            message="hi",
            provider="codex",
            cwd=cwd,
            once=True,
            timeout=10,
        )
        py_exit = 0
    except DispatchAskError as exc:
        py_exit = exc.exit_code

    # Rust side: separate home, same env shape.
    rs_home = tmp_path / "rs-home"
    rs_home.mkdir()
    env = {
        **os.environ,
        "FNO_AGENTS_HOME": str(rs_home),
        "PATH": fake_path,
        "FAKE_CODEX_EXIT": "1",
    }
    env.pop("FAKE_CODEX_SESSION_ID", None)
    env.pop("FAKE_CODEX_REPLY", None)

    completed = subprocess.run(
        [str(RUST_BIN), "spawn", "parity-agent", "hi", "--provider", "codex", "--once"],
        env=env,
        capture_output=True,
        text=True,
        timeout=30,
    )

    assert py_exit != 0, "Python path must propagate non-zero exit from failed codex"
    assert completed.returncode != 0, (
        f"Rust path must propagate non-zero exit from failed codex; got 0 (stdout: {completed.stdout!r})"
    )


# --------------------------------------------------------------------------- #
# Followup differential, parametrized across providers (cv-1314d0e7).
#
# The existing tests cover CREATE + failure propagation. cv-1314d0e7 noted the
# followup-library parity lives in codex_ask_parity.rs / gemini_ask_parity.rs
# but not at the CLI boundary. This matrix closes that gap and also gives gemini
# its first CLI-level e2e coverage after the unconditional flip (ab-73da4ac2):
# a follow-up ask (no `--provider`, resolved from the registry a prior create
# wrote) reaches the same fake provider with the same reply + exit on both the
# Python dispatch and the Rust client.
# --------------------------------------------------------------------------- #


@requires_rust
@pytest.mark.parametrize("provider", ["codex", "gemini"])
def test_ask_followup_python_vs_rust_parity(provider, tmp_path: Path, monkeypatch) -> None:
    """A follow-up ask produces the same stdout + exit on the Python dispatch and
    the Rust client, for codex AND gemini. The agent is seeded via the retained
    create machinery (Task 1.3: ask never creates), and the Rust side reads the
    Python-written registry row - exactly the production shape, where both
    runtimes share one registry. Follow-ups pass no `--provider` so the
    provider is resolved from the registry row."""
    import shutil

    install_fake, env_for = _PROVIDER_FAKES[provider]
    bin_dir = tmp_path / "bin"
    install_fake(bin_dir)
    fake_path = f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}"
    create_reply = "create reply"
    followup_reply = "followup reply"

    cwd = tmp_path / "workdir"
    cwd.mkdir()

    # --- Python path: seed via the retained create helper (writes the
    # registry row), then followup (resolve via registry).
    py_home = tmp_path / "py-home"
    py_home.mkdir()
    use_tmpdir(monkeypatch, py_home)
    monkeypatch.setenv("PATH", fake_path)

    from fno import paths
    from fno.agents.dispatch import (
        _codex_create_path,
        _gemini_create_path,
        dispatch_ask,
    )

    class _Lock:
        def detach(self) -> None:
            pass

    create_helper = _codex_create_path if provider == "codex" else _gemini_create_path
    for k, v in env_for(_SESSION_ID, create_reply).items():
        monkeypatch.setenv(k, v)
    create_helper(
        name="fu-agent",
        message="hi",
        cwd=cwd,
        from_name="fno",
        yolo=False,
        timeout_sec=10.0,
        lock_handle=_Lock(),
    )

    for k, v in env_for(_SESSION_ID, followup_reply).items():
        monkeypatch.setenv(k, v)
    py_fu = dispatch_ask(name="fu-agent", message="again", provider=None, cwd=cwd, timeout=10)
    py_stdout = py_fu.reply or ""  # followup -> reply verbatim, no trailing newline

    # --- Rust path: separate home seeded by COPYING the Python-written
    # registry (cross-language read is the production contract; both runtimes
    # share ~/.fno/agents/registry.json). Run from the same cwd so the
    # followup honors the registry-pinned cwd (gemini/codex resume is
    # cwd-pinned).
    rs_home = tmp_path / "rs-home"
    rs_home.mkdir()
    shutil.copy(paths.agents_registry_path(), rs_home / "registry.json")
    base_env = {**os.environ, "FNO_AGENTS_HOME": str(rs_home), "PATH": fake_path}

    rs = subprocess.run(
        [str(RUST_BIN), "ask", "fu-agent", "again"],
        env={**base_env, **env_for(_SESSION_ID, followup_reply)},
        cwd=cwd, capture_output=True, text=True, timeout=30, check=False,
    )

    assert rs.returncode == 0, f"rust {provider} followup failed: {rs.stderr}"
    assert py_stdout == rs.stdout, (
        f"{provider} followup stdout drift:\n"
        f"  python: {py_stdout!r}\n"
        f"  rust:   {rs.stdout!r}\n"
        f"  rust stderr: {rs.stderr}"
    )
