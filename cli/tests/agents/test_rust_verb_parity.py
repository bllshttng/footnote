"""Per-verb byte-parity tests for the Rust port of the Python-only agents verbs
(ab-d82655d7, AC3-EDGE).

Each promoted verb has a Python implementation (the ``FNO_AGENTS_RUNTIME=python``
fallback) and a Rust client implementation (the default ``auto`` route). The
promotion gate (Locked Decision #3) is byte-for-byte stdout + matching exit code.
These tests run each verb on the Rust binary against a fixture state tree and
assert the output equals the Python implementation's for the same fixture.

The Python side is exercised via its pure ``*_logic`` functions / the CLI
echo path, which produce exactly the bytes the ``fno agents <verb>`` Python
command writes (the Typer wrapper only does ``sys.stdout.write(result.output)``).
The Rust side is the compiled ``fno-agents`` client driven with ``FNO_AGENTS_HOME``
pointed at the fixture. Skipped when the Rust binary is absent (sdist test env).
"""
from __future__ import annotations

import functools
import json
import os
import shutil
import subprocess
import tempfile
from pathlib import Path
from types import SimpleNamespace

import pytest


@functools.lru_cache(maxsize=1)
def _fno_shim_dir() -> str | None:
    """A PATH dir providing ``fno``, or ``None`` when the real one is present.

    The Rust client resolves a short token by shelling out to
    ``fno agents heal-token --all-sources`` (``client_verbs.rs``). ``fno`` is the
    cargo-installed RUST binary; the Python package installs only ``fno-py``
    (``[project.scripts]``). So a checkout that has never run ``cargo install``
    -- CI, and any dev box that only built ``fno-agents`` -- has no ``fno`` on
    PATH, the shellout dies with ENOENT, and every short-token case fails the
    Rust/Python parity assertion for a reason that is purely environmental.

    The shim resolves ``fno-py`` at exec time rather than install time, because
    it is the surrounding ``uv run`` that puts the venv bin on PATH.
    """
    if shutil.which("fno"):
        return None
    d = tempfile.mkdtemp(prefix="fno-parity-shim-")
    shim = Path(d) / "fno"
    shim.write_text('#!/bin/sh\nexec fno-py "$@"\n')
    shim.chmod(0o755)
    return d


def _find_rust_bin() -> Path | None:
    """Locate the compiled ``fno-agents`` client (release preferred, then debug).

    Walks up to the workspace root (the ancestor with ``crates/fno-agents``),
    mirroring ``test_rust_runtime._find_repo_root``.
    """
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


def _run_rust(args: list[str], home: Path) -> subprocess.CompletedProcess:
    """Run the Rust client with FNO_AGENTS_HOME pointed at ``home`` (the agents dir).

    The full environment (notably ``PATH``) is inherited so the Rust client's
    provider-CLI PATH check sees the same PATH as Python's ``shutil.which`` --
    otherwise resume/attach would diverge purely on which PATH each side was
    handed, not on behavior.
    """
    source_path = str(Path(__file__).resolve().parents[2] / "src")
    inherited_pythonpath = os.environ.get("PYTHONPATH")
    pythonpath = (
        source_path + os.pathsep + inherited_pythonpath
        if inherited_pythonpath
        else source_path
    )
    env = {
        **os.environ,
        "FNO_AGENTS_HOME": str(home),
        "PYTHONPATH": pythonpath,
    }
    # Only when the real binary is absent, so a machine that HAS fno keeps
    # exercising the genuine article rather than a shim.
    shim_dir = _fno_shim_dir()
    if shim_dir:
        env["PATH"] = shim_dir + os.pathsep + env.get("PATH", "")
    # A short-token parity case asks the Rust client to invoke the Python
    # all-source resolver. Keep that test hermetic instead of scanning the
    # operator's real Claude/OpenCode corpora; individual tests can still
    # override these seams through monkeypatch before calling this helper.
    isolated_stores = {
        "FNO_CLAUDE_PROJECTS_DIR": home.parent / "empty-claude-projects",
        "FNO_CODEX_SESSIONS_DIR": home.parent / "empty-codex-sessions",
        "FNO_OPENCODE_STORAGE_DIR": home.parent / "empty-opencode" / "storage",
    }
    for key, path in isolated_stores.items():
        if key not in env:
            path.mkdir(parents=True, exist_ok=True)
            env[key] = str(path)
    return subprocess.run(
        [str(RUST_BIN), *args],
        env=env,
        capture_output=True,
        text=True,
    )


# --------------------------------------------------------------------------- #
# ping -- verbatim stub
# --------------------------------------------------------------------------- #

@requires_rust
def test_ping_parity(tmp_path) -> None:
    rust = _run_rust(["ping"], tmp_path / "agents")
    # Python cmd_ping: typer.echo("(not yet implemented; planned for a future story)")
    assert rust.stdout == "(not yet implemented; planned for a future story)\n"
    assert rust.returncode == 0


# --------------------------------------------------------------------------- #
# drive-authority
# --------------------------------------------------------------------------- #

def _seed_state(agents: Path, short_id: str, *, drive_active: bool, mode: str, sid: str) -> None:
    d = agents / short_id
    d.mkdir(parents=True, exist_ok=True)
    (d / "state.json").write_text(
        json.dumps(
            {
                "schema_version": 1,
                "short_id": short_id,
                "status": "ready",
                "pty": {
                    "active": True,
                    "drive_active": drive_active,
                    "drive_mode": mode,
                    "drive_session_id": sid,
                },
            }
        )
    )


@requires_rust
@pytest.mark.parametrize("json_flag", [True, False])
def test_drive_authority_parity(tmp_path, json_flag) -> None:
    from fno.drive_authority import active_drive_sessions

    agents = tmp_path / "agents"
    _seed_state(agents, "wkI", drive_active=True, mode="interactive", sid="d-1")
    _seed_state(agents, "wkW", drive_active=True, mode="watch", sid="d-2")  # excluded
    _seed_state(agents, "wkS", drive_active=True, mode="step", sid="d-3")

    sessions = active_drive_sessions(agents)
    if json_flag:
        expected_out = json.dumps({"active": bool(sessions), "sessions": sessions}) + "\n"
        args = ["drive-authority", "--json"]
    else:
        expected_out = "".join(f"{s['short_id']} {s['mode']} {s['session_id']}\n" for s in sessions)
        args = ["drive-authority"]
    expected_exit = 0 if sessions else 1

    rust = _run_rust(args, agents)
    assert rust.stdout == expected_out
    assert rust.returncode == expected_exit


@requires_rust
def test_drive_authority_none_parity(tmp_path) -> None:
    agents = tmp_path / "agents"
    agents.mkdir(parents=True)
    rust = _run_rust(["drive-authority"], agents)
    assert rust.stdout == "no active drive authority\n"
    assert rust.returncode == 1
    rust_json = _run_rust(["drive-authority", "--json"], agents)
    assert rust_json.stdout == '{"active": false, "sessions": []}\n'
    assert rust_json.returncode == 1


# --------------------------------------------------------------------------- #
# trace
# --------------------------------------------------------------------------- #

@requires_rust
@pytest.mark.parametrize("json_flag", [True, False])
def test_trace_parity(tmp_path, json_flag) -> None:
    from fno.agents.trace_cli import trace_logic

    agents = tmp_path / "agents"
    agents.mkdir(parents=True)
    events = tmp_path / "events.jsonl"  # state_dir/events.jsonl == agents.parent/events.jsonl
    events.write_text(
        '{"ts":"2026-05-26T10:00:00Z","kind":"agent_ask_started","from_name":"fno","to_name":"w","request_id":"a1b2c3d4e5f600000000000000000aaa","caller_kind":"fno"}\n'
        '{"ts":"2026-05-26T10:00:05Z","kind":"agent_ask_done","from_name":"fno","to_name":"w","request_id":"a1b2c3d4e5f600000000000000000aaa","caller_kind":"fno"}\n'
        '{"ts":"2026-05-26T10:01:00Z","kind":"agent_ask_started","from_name":"fno","to_name":"w","request_id":"ffffffff0000000000000000000000bb","caller_kind":"fno"}\n'
    )
    # Rust resolves the registry at agents/registry.json; seed it so the agent
    # passes the membership gate (Python side skips the check via registry_check).
    (agents / "registry.json").write_text(
        json.dumps(
            {
                "schema_version": 3,
                "agents": [
                    {
                        "name": "w",
                        "short_id": "w",
                        "provider": "codex",
                        "cwd": "/tmp/x",
                        "project_root": "/tmp/x",
                        "log_path": "/tmp/x/l.jsonl",
                        "status": "live",
                        "created_at": "2026-05-26T09:00:00Z",
                    }
                ],
            }
        )
    )
    py = trace_logic(name="w", json_out=json_flag, events_path=events, registry_check=False)
    args = ["trace", "w"] + (["--json"] if json_flag else [])
    rust = _run_rust(args, agents)
    assert rust.stdout == py.output
    assert rust.returncode == py.exit_code


@requires_rust
def test_trace_name_required_parity(tmp_path) -> None:
    from fno.agents.trace_cli import trace_logic

    agents = tmp_path / "agents"
    agents.mkdir(parents=True)
    py = trace_logic(name=None, events_path=tmp_path / "events.jsonl")
    rust = _run_rust(["trace"], agents)
    assert rust.returncode == 2 == py.exit_code
    assert rust.stderr == py.stderr


# --------------------------------------------------------------------------- #
# resume (--print-command + error paths)
# --------------------------------------------------------------------------- #

def _seed_registry(agents: Path, entries: list[dict]) -> None:
    agents.mkdir(parents=True, exist_ok=True)
    (agents / "registry.json").write_text(json.dumps({"schema_version": 3, "agents": entries}))


@requires_rust
def test_resume_print_command_parity(tmp_path) -> None:
    from fno.agents import resume_cli

    entries = [
        {
            "name": "cx",
            "short_id": "cx",
            "harness": "codex",
            "cwd": "/tmp/proj space",
            "project_root": "/tmp/proj space",
            "log_path": "/tmp/proj space/l.jsonl",
            "harness_session_id": "uuid-1",
            "status": "live",
            "created_at": "2026-05-26T09:00:00Z",
        }
    ]
    agents = tmp_path / "agents"
    _seed_registry(agents, entries)

    def loader():
        return [SimpleNamespace(**e) for e in entries]

    py = resume_cli.resume_logic(name="cx", print_command=True, registry_loader=loader)
    rust = _run_rust(["resume", "cx", "--print-command"], agents)
    assert rust.stdout == py.output
    assert rust.returncode == py.exit_code


@requires_rust
def test_resume_gemini_print_command_parity(tmp_path) -> None:
    from fno.agents import resume_cli

    entries = [
        {
            "name": "gm",
            "short_id": "gm",
            "harness": "gemini",
            "cwd": "/tmp/proj",
            "project_root": "/tmp/proj",
            "log_path": "/tmp/proj/l.jsonl",
            "harness_session_id": "gem-session-1",
            "status": "live",
            "created_at": "2026-05-26T09:00:00Z",
        }
    ]
    agents = tmp_path / "agents"
    _seed_registry(agents, entries)

    def loader():
        return [SimpleNamespace(**e) for e in entries]

    py = resume_cli.resume_logic(
        name="gm", print_command=True, registry_loader=loader
    )
    rust = _run_rust(["resume", "gm", "--print-command"], agents)
    assert rust.stdout == py.output == "cd /tmp/proj && exec gemini --resume gem-session-1\n"
    assert rust.returncode == py.exit_code == 0


@requires_rust
def test_resume_resolves_by_short_and_full_id_parity(tmp_path) -> None:
    """Rust and Python agree on every current and compatibility address form."""
    from fno.agents import resume_cli

    full = "a1b2c3d4-1111-2222-3333-444455556666"
    entries = [
        {
            "name": "cx",
            "short_id": "cxworker",  # daemon name-derived key (not the uuid prefix)
            "harness": "codex",
            "cwd": "/tmp/proj",
            "project_root": "/tmp/proj",
            "log_path": "/tmp/proj/l.jsonl",
            "harness_session_id": full,
            "status": "live",
            "created_at": "2026-05-26T09:00:00Z",
        }
    ]
    agents = tmp_path / "agents"
    _seed_registry(agents, entries)

    def loader():
        return [SimpleNamespace(**e) for e in entries]

    by_name = _run_rust(["resume", "cx", "--print-command"], agents)
    assert by_name.returncode == 0, by_name.stderr
    for token in ("55556666", "a1b2c3d4", full, "cxworker"):
        rust = _run_rust(["resume", token, "--print-command"], agents)
        assert rust.stdout == by_name.stdout, f"token {token} diverged"
        assert rust.returncode == 0
        py = resume_cli.resume_logic(
            name=token, print_command=True, registry_loader=loader
        )
        assert rust.stdout == py.output, f"rust/py parity for token {token}"


@requires_rust
def test_resume_opencode_canonical_tail_case_parity(tmp_path) -> None:
    """The Rust promotion gate preserves OpenCode's case-sensitive tail."""
    from fno.agents import resume_cli

    full = "ses_7f3a9b2cAbCd1234"
    entries = [
        {
            "name": "oc",
            "short_id": "ocworker",
            "harness": "opencode",
            "cwd": "/tmp/proj",
            "project_root": "/tmp/proj",
            "log_path": "/tmp/proj/l.jsonl",
            "harness_session_id": full,
            "status": "live",
            "created_at": "2026-05-26T09:00:00Z",
        }
    ]
    agents = tmp_path / "agents"
    _seed_registry(agents, entries)

    def loader():
        return [SimpleNamespace(**entry) for entry in entries]

    # Warm the first-run Rust front door; dependency provisioning may write to
    # stderr, but it is not part of the resume contract compared below.
    warm = _run_rust(["resume", "AbCd1234", "--print-command"], agents)
    assert warm.returncode == 0, warm.stderr

    for token in ("oc", full, "AbCd1234", "ocworker", "abcd1234"):
        rust = _run_rust(["resume", token, "--print-command"], agents)
        py = resume_cli.resume_logic(
            name=token, print_command=True, registry_loader=loader
        )
        assert rust.returncode == py.exit_code, f"exit parity for token {token}"
        assert rust.stdout == py.output, f"stdout parity for token {token}"
        assert rust.stderr == py.stderr, f"stderr parity for token {token}"

    assert _run_rust(["resume", "AbCd1234", "--print-command"], agents).returncode == 0
    assert _run_rust(["resume", "abcd1234", "--print-command"], agents).returncode != 0


@requires_rust
@pytest.mark.parametrize(
    "name,expected_exit",
    [("nope", 13), ("no-cwd", 13), ("no-sid", 13)],
)
def test_resume_error_parity(tmp_path, name, expected_exit) -> None:
    from fno.agents import resume_cli

    entries = [
        {"name": "no-cwd", "harness": "codex", "cwd": "", "log_path": "/x/l", "harness_session_id": "u", "short_id": "a", "project_root": "/x", "status": "live", "created_at": "t"},
        {"name": "no-sid", "harness": "codex", "cwd": "/tmp/x", "log_path": "/x/l", "short_id": "b", "project_root": "/x", "status": "live", "created_at": "t"},
    ]
    agents = tmp_path / "agents"
    _seed_registry(agents, entries)

    def loader():
        return [SimpleNamespace(**e) for e in entries]

    py = resume_cli.resume_logic(name=name, print_command=True, registry_loader=loader)
    rust = _run_rust(["resume", name, "--print-command"], agents)
    assert rust.returncode == expected_exit == py.exit_code
    assert rust.stderr == py.stderr


# --------------------------------------------------------------------------- #
# attach (codex/gemini refused path -- claude path needs the claude CLI)
# --------------------------------------------------------------------------- #

@requires_rust
def test_attach_refuses_codex_parity(tmp_path) -> None:
    agents = tmp_path / "agents"
    _seed_registry(
        agents,
        [{"name": "cx", "provider": "codex", "cwd": "/tmp/x", "log_path": "/x/l", "short_id": "cx", "project_root": "/x", "status": "live", "created_at": "t"}],
    )
    rust = _run_rust(["attach", "cx"], agents)
    # Mirrors dispatch.attach_agent's one-shot refusal message + exit 13.
    expected = (
        "codex agents are one-shot; no persistent session to attach to. "
        "Use 'fno agents logs cx --follow' for live output. Cross-harness "
        "attach is planned for the Phase 6 supervisor.\n"
    )
    assert rust.stderr == expected
    assert rust.returncode == 13


@requires_rust
def test_attach_missing_agent_exit2(tmp_path) -> None:
    # attach uses the lifecycle convention: missing agent is exit 2 (NOT 13,
    # which is resume's convention). The shared resolver (x-1b1e) prints the
    # accepted-forms not-found message; the exit code is unchanged.
    agents = tmp_path / "agents"
    _seed_registry(agents, [])
    rust = _run_rust(["attach", "ghost"], agents)
    assert rust.stderr == (
        "no agent matching 'ghost'; "
        "accepted forms: name, canonical handle, transport short id, or full session id\n"
    )
    assert rust.returncode == 2


# --------------------------------------------------------------------------- #
# logs (codex one-shot file read)
# --------------------------------------------------------------------------- #

@requires_rust
@pytest.mark.parametrize("tail", [2, 0, 100])
@pytest.mark.parametrize("token", ["cx", "deadbeef"])
def test_logs_codex_oneshot_parity(tmp_path, monkeypatch, tail, token) -> None:
    from fno.agents import read as read_mod
    from fno.agents.registry import AgentEntry
    import io

    agents = tmp_path / "agents"
    agents.mkdir(parents=True)
    projects = tmp_path / "projects"
    codex = tmp_path / "codex"
    opencode_storage = tmp_path / "opencode" / "storage"
    projects.mkdir()
    codex.mkdir()
    opencode_storage.mkdir(parents=True)
    monkeypatch.setenv("FNO_CLAUDE_PROJECTS_DIR", str(projects))
    monkeypatch.setenv("FNO_CODEX_SESSIONS_DIR", str(codex))
    monkeypatch.setenv("FNO_OPENCODE_STORAGE_DIR", str(opencode_storage))
    log = agents / "cx.log.jsonl"
    log.write_text('{"line":1}\n{"line":2}\n{"line":3}\n{"line":4}')  # last line no newline
    session_id = "019fb417-1111-7222-8333-4444deadbeef"
    entries = [
        {
            "name": "cx",
            "short_id": "cx",
            "provider": "codex",
            "harness_session_id": session_id,
            "cwd": "/tmp/x",
            "project_root": "/tmp/x",
            "log_path": str(log),
            "status": "live",
            "created_at": "2026-05-26T09:00:00Z",
        }
    ]
    (agents / "registry.json").write_text(json.dumps({"schema_version": 3, "agents": entries}))

    # Python side: read_logs with load_registry pointed at the fixture.
    def fake_registry():
        return [
            AgentEntry(
                name=e["name"],
                harness=e["provider"],
                harness_session_id=e["harness_session_id"],
                cwd=e["cwd"],
                log_path=e["log_path"],
                created_at=e["created_at"],
                status=e["status"],
            )
            for e in entries
        ]

    orig = read_mod.load_registry
    read_mod.load_registry = fake_registry
    try:
        out = io.StringIO()
        err = io.StringIO()
        py = read_mod.read_logs(name=token, tail=tail, stdout=out, stderr=err)
        py_out = out.getvalue()
        py_exit = py.exit_code
    finally:
        read_mod.load_registry = orig

    rust = _run_rust(["logs", token, "--tail", str(tail)], agents)
    assert rust.stdout == py_out
    assert rust.returncode == py_exit


@requires_rust
def test_logs_missing_file_parity(tmp_path) -> None:
    from fno.agents import read as read_mod
    from fno.agents.registry import AgentEntry
    import io

    agents = tmp_path / "agents"
    absent = tmp_path / "absent.jsonl"
    entries = [{"name": "cx", "provider": "codex", "cwd": "/tmp/x", "short_id": "cx", "project_root": "/x", "log_path": str(absent), "status": "live", "created_at": "t"}]
    _seed_registry(agents, entries)

    # Python side: read_logs against the same fixture row.
    def fake_registry():
        return [
            AgentEntry(name=e["name"], harness=e["provider"], cwd=e["cwd"], log_path=e["log_path"], created_at=e["created_at"], status=e["status"])
            for e in entries
        ]

    orig = read_mod.load_registry
    read_mod.load_registry = fake_registry
    try:
        out = io.StringIO()
        err = io.StringIO()
        py = read_mod.read_logs(name="cx", stdout=out, stderr=err)
        py_err = err.getvalue()
        py_exit = py.exit_code
    finally:
        read_mod.load_registry = orig

    rust = _run_rust(["logs", "cx"], agents)
    # The honest "no log file" message (ab-65c3e60d) replaces the old US4 stub;
    # codex/gemini log retrieval is implemented (see test_logs_codex_oneshot_parity).
    assert rust.stderr == py_err
    assert rust.stderr == f"no logs for codex agent cx: no log file at {absent}\n"
    assert rust.returncode == py_exit == 13


@requires_rust
def test_logs_agent_not_found_parity(tmp_path) -> None:
    agents = tmp_path / "agents"
    _seed_registry(agents, [])
    rust = _run_rust(["logs", "ghost"], agents)
    assert rust.stderr == (
        "no agent matching 'ghost'; "
        "accepted forms: name, canonical handle, transport short id, or full session id\n"
    )
    assert rust.returncode == 13


# --------------------------------------------------------------------------- #
# Contract: the Rust client must read a registry written by Python's REAL
# write_registry (top-level key "agents"), not a hand-built fixture. This is the
# round-trip that would have caught the entries-vs-agents key bug -- the earlier
# fixtures wrote the key the Rust code happened to read.
# --------------------------------------------------------------------------- #

@requires_rust
def test_rust_reads_real_python_written_registry(tmp_path) -> None:
    from fno.agents.registry import AgentEntry, write_registry

    agents = tmp_path / "agents"
    agents.mkdir(parents=True)
    write_registry(
        [
            AgentEntry(
                name="cx",
                harness="codex",
                cwd="/tmp/proj",
                log_path=str(agents / "cx.jsonl"),
                harness_session_id="uuid-9",
            )
        ],
        agents / "registry.json",
    )
    # resume resolves the agent (codex resume argv) -> exit 0 with the snippet,
    # proving the Rust reader found the agent under the real "agents" key.
    rust = _run_rust(["resume", "cx", "--print-command"], agents)
    assert rust.returncode == 0, rust.stderr
    assert rust.stdout == "cd /tmp/proj && exec codex resume uuid-9\n"
    # attach refuses codex (agent FOUND -> the refusal message, not "not found").
    att = _run_rust(["attach", "cx"], agents)
    assert att.returncode == 13
    assert "one-shot" in att.stderr
