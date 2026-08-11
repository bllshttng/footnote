"""Tests for the mux-pane spawn back half (4a-G2, task 4.5).

``fno agents spawn --substrate pane`` hosts the agent as a mux pane via
``fno mux pane run`` and writes the registry row with the ``mux`` ref. The
mux subprocess is faked at the ``runner`` seam (the G1 e2e drives the real
socket); these tests pin the Python contract:

- AC1-HP  spawn -> `pane run --session --cwd -- env <mesh> <argv>`; row
          carries mux:{session, pane_id} + claude_session_uuid + child pid.
- AC1-ERR mux failure -> no half-created row, error names the mux session,
          no daemon fallback.
- AC1-FR  a claude argv carrying -p/--print is refused BEFORE any pane run.
- AC1-EDGE is Rust-side (pane run self-spawns the server; G1 e2e covers it).
- Routing: pane-substrate spawns never auto-route to the Rust client.
"""
from __future__ import annotations

import io
import json
import os
import shutil
import subprocess
import time
import uuid
from pathlib import Path
from typing import Optional

import pytest

from fno.paths_testing import use_tmpdir


class FakeRunner:
    """Record `fno mux ...` invocations; script the replies per verb."""

    def __init__(
        self,
        run_returncode: int = 0,
        run_stdout: str = "7\n",
        run_stderr: str = "",
        ls_stdout: Optional[str] = None,
        db_stdout: str = "",
        # x-6928 interactive-readiness gate probes.
        wait_returncode: int = 11,
        read_stdout: str = "",
        read_returncode: int = 0,
        read_stderr: str = "",
        placement: Optional[dict] = None,
        kill_returncode: int = 0,
        kill_stderr: str = "",
        kill_exception: Optional[Exception] = None,
    ) -> None:
        self.calls: list[list[str]] = []
        self.run_returncode = run_returncode
        self.run_stdout = run_stdout
        self.run_stderr = run_stderr
        self.ls_stdout = ls_stdout
        self.db_stdout = db_stdout
        self.wait_returncode = wait_returncode
        self.read_stdout = read_stdout
        self.read_returncode = read_returncode
        self.read_stderr = read_stderr
        self.placement = placement
        self.kill_calls: list[list[str]] = []
        self.kill_returncode = kill_returncode
        self.kill_stderr = kill_stderr
        self.kill_exception = kill_exception

    def __call__(self, argv, **kwargs):
        self.calls.append(list(argv))
        # The opencode spawn path reads opencode's session store through this
        # same seam (x-830c); default empty output = "no session captured", the
        # live-only row every non-opencode test already expects.
        if argv[:2] == ["opencode", "db"]:
            return subprocess.CompletedProcess(argv, 0, self.db_stdout, "")
        if argv[1:4] == ["mux", "pane", "run"]:
            if "--json" in argv:
                payload = {"pane_id": 7}
                if self.placement is not None:
                    payload["placement"] = self.placement
                return subprocess.CompletedProcess(
                    argv, self.run_returncode, json.dumps(payload), self.run_stderr
                )
            return subprocess.CompletedProcess(
                argv, self.run_returncode, self.run_stdout, self.run_stderr
            )
        if argv[1:4] == ["mux", "pane", "ls"]:
            out = self.ls_stdout
            if out is None:
                out = json.dumps(
                    [{"pane_id": 7, "squad_id": 1, "tab_id": 1, "cwd": "/w", "child_pid": 4242}]
                )
            return subprocess.CompletedProcess(argv, 0, out, "")
        if argv[1:4] == ["mux", "pane", "wait"]:
            return subprocess.CompletedProcess(argv, self.wait_returncode, "", "")
        if argv[1:4] == ["mux", "pane", "read"]:
            return subprocess.CompletedProcess(
                argv, self.read_returncode, self.read_stdout, self.read_stderr
            )
        if argv[1:4] == ["mux", "pane", "kill"]:
            self.kill_calls.append(list(argv))
            if self.kill_exception is not None:
                raise self.kill_exception
            return subprocess.CompletedProcess(
                argv, self.kill_returncode, "", self.kill_stderr
            )
        raise AssertionError(f"unexpected fno invocation: {argv}")


def _spawn(monkeypatch, tmp_path, **kwargs):
    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.delenv("FNO_SESSION", raising=False)
    from fno.agents.mux_spawn import dispatch_spawn_pane

    runner = kwargs.pop("runner", FakeRunner())
    result = dispatch_spawn_pane(
        name=kwargs.pop("name", "peer"),
        message=kwargs.pop("message", "hello"),
        provider=kwargs.pop("provider", "claude"),
        cwd=kwargs.pop("cwd", tmp_path),
        runner=runner,
        **kwargs,
    )
    return result, runner


def test_late_codex_identity_composes_across_every_peer_surface(
    tmp_path: Path, monkeypatch
) -> None:
    """One derived pane identity reaches every public peer surface unchanged."""
    use_tmpdir(monkeypatch, tmp_path)
    repo = Path(__file__).resolve().parents[3]
    manifest = repo / "crates" / "fno" / "Cargo.toml"
    fno_bin = repo / "crates" / "fno" / "target" / "debug" / "fno"
    cargo_path = shutil.which("cargo")
    if cargo_path is None:
        # This is the strongest test in the suite and it drives the real fno
        # binary, so it needs a toolchain. Skip where there is none rather than
        # hard-erroring: an environment without cargo has nothing to say about
        # this invariant, and a red that means "no rust here" trains people to
        # ignore reds.
        pytest.skip("cargo not on PATH; this journey drives the real fno binary")
    cargo = Path(cargo_path)
    cargo_home = cargo.parent.parent
    build_env = {
        **os.environ,
        "CARGO_HOME": str(cargo_home),
        "RUSTUP_HOME": str(cargo_home.parent / ".rustup"),
    }
    built = subprocess.run(
        [str(cargo), "build", "--manifest-path", str(manifest), "--bin", "fno"],
        cwd=repo,
        env=build_env,
        text=True,
        capture_output=True,
    )
    assert built.returncode == 0, built.stderr

    agents_home = tmp_path / ".fno" / "agents"
    mux_dir = Path("/tmp") / f"fno-i-{os.getpid()}-{uuid.uuid4().hex[:6]}"
    mux_dir.mkdir()
    monkeypatch.setenv("FNO_BIN", str(fno_bin))
    monkeypatch.setenv("FNO_AGENTS_HOME", str(agents_home))
    monkeypatch.setenv("FNO_MUX_DIR", str(mux_dir))
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claim-root"))
    monkeypatch.setenv("FNO_E2E", "1")
    monkeypatch.delenv("FNO_SESSION", raising=False)

    requested_name = "late-codex-identity"
    mux_session = "identity-journey"
    rollout_root = tmp_path / "rollouts"
    rollout_root.mkdir()
    rollout = rollout_root / f"rollout-{uuid.uuid4()}.jsonl"
    rollout.write_text(
        "\n".join(
            [
                json.dumps(
                    {
                        "type": "session_meta",
                        "payload": {"id": str(uuid.uuid4()), "cwd": str(repo)},
                    }
                ),
                json.dumps(
                    {
                        "type": "response_item",
                        "payload": {
                            "type": "message",
                            "role": "assistant",
                            "content": [{"type": "output_text", "text": "READY"}],
                        },
                    }
                ),
            ]
        )
        + "\n",
        encoding="utf-8",
    )

    from fno.agents import dispatch, mux_spawn
    from fno.agents.discover import resolve_or_suggest
    from fno.agents.peek import peek
    from fno.agents.registry import load_registry, resolve_agent
    from fno.claims.core import acquire_claim

    original_argv = mux_spawn.build_pane_argv
    original_capture = mux_spawn._backfill_codex_session_id
    monkeypatch.setattr(
        mux_spawn,
        "build_pane_argv",
        lambda *_args, **_kwargs: [
            "/bin/sh",
            "-c",
            'exec 3<"$1"; sleep 60',
            "sh",
            str(rollout),
        ],
    )
    monkeypatch.setattr(
        mux_spawn, "_backfill_codex_session_id", lambda *_args, **_kwargs: None
    )

    spawned = None
    try:
        spawned = mux_spawn.dispatch_spawn_pane(
            name=requested_name,
            message="wait",
            provider="codex",
            cwd=repo,
            session=mux_session,
        )
        assert spawned.status == "spawning"
        assert spawned.session_uuid is None
        assert spawned.short_id == ""

        monkeypatch.setattr(mux_spawn, "build_pane_argv", original_argv)
        monkeypatch.setattr(
            mux_spawn, "_backfill_codex_session_id", original_capture
        )
        # The pane child opens the rollout on fd 3 only after it execs, and the
        # heal correlates on exactly that open fd. Reconciling before it is open
        # observes a legitimate "pending" and proves nothing, so wait for the
        # precondition instead of assuming the spawn won the race: this test
        # passed serially and failed only under parallel load, where child
        # startup is the thing that slips.
        probe_pid = load_registry(path=agents_home / "registry.json")[0].pid
        deadline = time.monotonic() + 30.0
        opened = None
        while time.monotonic() < deadline:
            opened = mux_spawn._codex_session_id_for_pid(probe_pid)
            if opened:
                break
            time.sleep(0.05)
        assert opened, (
            f"pane child pid={probe_pid} never opened its rollout within 30s; "
            "the late-identity heal correlates on that open fd"
        )

        reconciled = dispatch.reconcile_agents(
            codex_session_index_path=tmp_path / "missing-index.jsonl"
        )
        assert len(reconciled.backfilled) == 1
        identity = reconciled.backfilled[0]["harness_session_id"]

        registry_path = agents_home / "registry.json"
        row = load_registry(path=registry_path)[0]
        assert row.harness_session_id == identity
        assert row.status == "live"
        assert resolve_agent(requested_name, path=registry_path).entry == row

        def resolver(handle):
            return resolve_or_suggest(
                handle,
                registry_path=registry_path,
                require_alive=False,
                sessions_dir=tmp_path / "no-claude",
                projects_dir=tmp_path / "no-projects",
                codex_sessions_dir=rollout_root,
                opencode_storage_dir=tmp_path / "no-opencode",
                name_map_path=tmp_path / "no-names.json",
                project_resolver=lambda _cwd: None,
            )

        resolved = []
        for handle in (requested_name, identity[:8], identity):
            peer, suggestions = resolver(handle)
            assert suggestions == []
            assert peer is not None
            resolved.append(peer.session_id)

        pane_ls = subprocess.run(
            [str(fno_bin), "mux", "pane", "ls", "--session", mux_session, "--json"],
            cwd=repo,
            text=True,
            capture_output=True,
            check=True,
        )
        pane = next(
            item
            for item in json.loads(pane_ls.stdout)
            if item["pane_id"] == spawned.pane_id
        )
        assert pane["fno_id"] == identity
        located = subprocess.run(
            [str(fno_bin), "mux", "where", identity, "--session", mux_session, "--json"],
            cwd=repo,
            text=True,
            capture_output=True,
            check=True,
        )
        assert json.loads(located.stdout)["panes"] == [spawned.pane_id]

        claim = acquire_claim(
            "node:ab-acde1234",
            identity,
            pid=row.pid,
            root=tmp_path / "claim-root",
        )
        assert claim.holder == identity

        observed, errors = io.StringIO(), io.StringIO()
        assert (
            peek(
                requested_name,
                stdout=observed,
                stderr=errors,
                resolve=resolver,
                codex_sessions_dir=rollout_root,
            )
            == 0
        )
        assert errors.getvalue() == ""
        assert "assistant: READY" in observed.getvalue()
        assert {row.harness_session_id, pane["fno_id"], claim.holder, *resolved} == {
            identity
        }
    finally:
        monkeypatch.setattr(mux_spawn, "build_pane_argv", original_argv)
        monkeypatch.setattr(
            mux_spawn, "_backfill_codex_session_id", original_capture
        )
        if spawned is not None:
            subprocess.run(
                [
                    str(fno_bin),
                    "mux",
                    "pane",
                    "kill",
                    "--session",
                    mux_session,
                    str(spawned.pane_id),
                ],
                cwd=repo,
                text=True,
                capture_output=True,
            )
        subprocess.run(
            [str(fno_bin), "mux", "kill-server", mux_session, "--json"],
            cwd=repo,
            text=True,
            capture_output=True,
        )
        shutil.rmtree(mux_dir, ignore_errors=True)


def test_codex_autonomous_pane_journey_completes_without_operator_input(
    tmp_path: Path, monkeypatch
) -> None:
    """A fake Codex pane receives its task, exits, and leaves readable output."""
    use_tmpdir(monkeypatch, tmp_path)
    repo = Path(__file__).resolve().parents[3]
    manifest = repo / "crates" / "fno" / "Cargo.toml"
    fno_bin = repo / "crates" / "fno" / "target" / "debug" / "fno"
    cargo_path = shutil.which("cargo")
    if cargo_path is None:
        pytest.skip("cargo not on PATH; this journey drives the real fno binary")
    cargo = Path(cargo_path)
    cargo_home = cargo.parent.parent
    built = subprocess.run(
        [str(cargo), "build", "--manifest-path", str(manifest), "--bin", "fno"],
        cwd=repo,
        env={
            **os.environ,
            "CARGO_HOME": str(cargo_home),
            "RUSTUP_HOME": str(cargo_home.parent / ".rustup"),
        },
        text=True,
        capture_output=True,
    )
    assert built.returncode == 0, built.stderr

    fake_bin = tmp_path / "bin"
    fake_bin.mkdir()
    fake_codex = fake_bin / "codex"
    fake_codex.write_text(
        """#!/bin/sh
for arg in "$@"; do prompt="$arg"; done
printf '%s' "$prompt" > "$FAKE_CODEX_PROMPT_FILE"
printf '\033]133;C\aAUTONOMOUS-CODEX-DONE\n\033]133;D;0\a'
sleep 5
""",
        encoding="utf-8",
    )
    fake_codex.chmod(0o755)

    agents_home = tmp_path / "agents"
    mux_dir = Path("/tmp") / f"fno-a-{os.getpid()}-{uuid.uuid4().hex[:6]}"
    mux_dir.mkdir()
    prompt_file = tmp_path / "received-prompt"
    session = f"auto-{uuid.uuid4().hex[:6]}"
    env = {
        **os.environ,
        "PATH": f"{fake_bin}{os.pathsep}{os.environ['PATH']}",
        "FNO_BIN": str(fno_bin),
        "FNO_AGENTS_HOME": str(agents_home),
        "FNO_MUX_DIR": str(mux_dir),
        "FNO_CLAIMS_ROOT": str(tmp_path / "claims"),
        "FNO_E2E": "1",
        "FAKE_CODEX_PROMPT_FILE": str(prompt_file),
    }
    for key, value in env.items():
        monkeypatch.setenv(key, value)
    monkeypatch.delenv("FNO_SESSION", raising=False)

    keeper = subprocess.run(
        [
            str(fno_bin),
            "mux",
            "pane",
            "run",
            "--session",
            session,
            "--cwd",
            str(repo),
            "--",
            "/bin/sh",
            "-c",
            "sleep 60",
        ],
        cwd=repo,
        env=env,
        text=True,
        capture_output=True,
    )
    assert keeper.returncode == 0, keeper.stderr

    from fno.agents.mux_spawn import dispatch_spawn_pane

    spawned = None
    try:
        spawned = dispatch_spawn_pane(
            name="autonomous-codex-proof",
            message="AUTONOMOUS-PANE-TASK",
            provider="codex",
            cwd=repo,
            session=session,
            codex_sessions_dir=tmp_path / "no-rollouts",
        )

        deadline = time.monotonic() + 10.0
        while time.monotonic() < deadline and not prompt_file.exists():
            time.sleep(0.05)
        assert prompt_file.read_text(encoding="utf-8") == "AUTONOMOUS-PANE-TASK"

        settled = subprocess.run(
            [
                str(fno_bin),
                "mux",
                "pane",
                "wait",
                "--session",
                session,
                str(spawned.pane_id),
                "--pattern",
                "AUTONOMOUS-CODEX-DONE",
                "--timeout",
                "10",
            ],
            cwd=repo,
            env=env,
            text=True,
            capture_output=True,
        )
        assert settled.returncode == 10, settled.stderr
        observed = subprocess.run(
            [
                str(fno_bin),
                "mux",
                "pane",
                "read",
                "--session",
                session,
                str(spawned.pane_id),
            ],
            cwd=repo,
            env=env,
            text=True,
            capture_output=True,
        )
        assert observed.returncode == 0, observed.stderr
        assert "AUTONOMOUS-CODEX-DONE" in observed.stdout

        observed_exit = subprocess.run(
            [
                str(fno_bin),
                "mux",
                "pane",
                "wait",
                "--session",
                session,
                str(spawned.pane_id),
                "--timeout",
                "10",
            ],
            cwd=repo,
            env=env,
            text=True,
            capture_output=True,
        )
        assert observed_exit.returncode == 12, observed_exit.stderr
    finally:
        subprocess.run(
            [str(fno_bin), "mux", "kill-server", session, "--json"],
            cwd=repo,
            env=env,
            text=True,
            capture_output=True,
        )
        shutil.rmtree(mux_dir, ignore_errors=True)


def test_opencode_spawn_stamps_the_captured_session_id(
    tmp_path: Path, monkeypatch
) -> None:
    """x-830c: a unique store match lands on the row as harness_session_id.

    This is what makes the opencode resume lane reachable at all - the mapping
    and the resume argv both key on this field.
    """
    from fno.agents.registry import load_registry

    ses = "ses_09679f284ffeJv7NdBAoLQLnLZ"
    result, _ = _spawn(
        monkeypatch, tmp_path,
        provider="opencode",
        runner=FakeRunner(db_stdout=f"id\n{ses}\n"),
    )
    rows = load_registry()
    assert [r.harness_session_id for r in rows] == [ses]
    # US8: opencode resumes off harness_session_id, so short_id stays empty -
    # the jobId population is claude-only. Assert BOTH the row and the
    # receipt-facing result: a regressed claude guard would otherwise hand out a
    # non-hex `ses_0967` result handle while the row-only check still passed.
    assert rows[0].short_id == ""
    assert result.short_id == ""


def test_opencode_spawn_never_claims_another_rows_session_id(
    tmp_path: Path, monkeypatch
) -> None:
    """Two panes racing in one cwd must not both stamp the same session id.

    The second pane's session may not exist yet when both backfills query, so
    each sees the SAME lone candidate and the ambiguity rule cannot fire. The
    loser drops to live-only rather than pointing resume at the other pane.
    """
    from fno.agents.registry import load_registry

    ses = "ses_09679f284ffeJv7NdBAoLQLnLZ"
    _spawn(
        monkeypatch, tmp_path, name="oc-a",
        provider="opencode", runner=FakeRunner(db_stdout=f"{ses}\n"),
    )
    # Second pane, same cwd, backfill returns the SAME id (the race).
    _spawn(
        monkeypatch, tmp_path, name="oc-b",
        provider="opencode", runner=FakeRunner(db_stdout=f"{ses}\n"),
    )
    rows = {r.name: r.harness_session_id for r in load_registry()}
    assert rows["oc-a"] == ses, "the first pane to land owns the id"
    assert rows["oc-b"] is None, "the loser must not share the id"


def test_opencode_spawn_without_capture_stays_live_only(
    tmp_path: Path, monkeypatch
) -> None:
    """AC2-FR: a missed capture leaves the row exactly as it was before x-830c.

    The pane itself is unaffected - the spawn still succeeds and reports its
    pane id; only resume is unavailable until an id is captured.
    """
    from fno.agents.registry import load_registry

    result, _ = _spawn(
        monkeypatch, tmp_path, provider="opencode", runner=FakeRunner(db_stdout=""),
    )
    assert result.pane_id == 7
    rows = load_registry()
    assert [r.harness_session_id for r in rows] == [None]


def test_codex_spawn_without_capture_stays_spawning(
    tmp_path: Path, monkeypatch
) -> None:
    """AC2-CON: an id-less Codex pane is created but not yet addressable."""
    from fno.agents.registry import load_registry

    result, _ = _spawn(monkeypatch, tmp_path, provider="codex")

    row = load_registry()[0]
    assert row.harness_session_id is None
    assert row.status == "spawning"
    assert result.status == "spawning"
    assert result.session_uuid is None
    assert result.short_id == ""


def test_codex_spawn_with_capture_returns_bound_identity(
    tmp_path: Path, monkeypatch
) -> None:
    """AC1-HP: the receipt and registry share the captured Codex thread ID."""
    from fno.agents import mux_spawn
    from fno.agents.registry import load_registry

    session_id = "019fb024-2327-75f3-8b80-06e9d5ade05f"
    monkeypatch.setattr(
        mux_spawn,
        "_backfill_codex_session_id",
        lambda *_args, **_kwargs: session_id,
    )

    result, _ = _spawn(monkeypatch, tmp_path, provider="codex")

    row = load_registry()[0]
    assert row.harness_session_id == session_id
    assert row.status == "live"
    assert result.status == "live"
    assert result.session_uuid == session_id
    assert result.short_id == session_id[:8]


def test_ac1_hp_spawn_pane_runs_mux_and_writes_mux_ref_row(
    tmp_path: Path, monkeypatch
) -> None:
    result, runner = _spawn(monkeypatch, tmp_path)

    # The hosting call is the G1 script API with the resolved session + cwd.
    run_call = runner.calls[0]
    assert run_call[1:4] == ["mux", "pane", "run"]
    assert "--claim" in run_call  # agent panes opt into the writer claim
    assert run_call[run_call.index("--session") + 1] == "main"
    assert run_call[run_call.index("--cwd") + 1] == str(tmp_path)
    # Mesh identity rides the env(1) wrapper after `--`.
    tail = run_call[run_call.index("--") + 1 :]
    assert tail[0] == "env"
    assert "FNO_AGENT_SELF=peer" in tail
    assert "FNO_AGENT_HARNESS=claude" in tail
    assert "CLAUDE_CODE_FORCE_SESSION_PERSISTENCE=1" in tail
    # The provider argv is interactive claude with the pinned session id and the
    # worker's own display name (x-c028: without `--name` claude inherits one
    # from the launching session's lineage and every pane worker looks alike).
    claude_at = tail.index("claude")
    assert tail[claude_at + 1] == "--session-id"
    assert tail[claude_at + 2] == result.session_uuid
    assert tail[claude_at + 3] == "--name"
    assert tail[claude_at + 4] == "peer"
    assert tail[claude_at + 5] == "hello"

    assert result.pane_id == 7
    assert result.session == "main"
    assert result.child_pid == 4242

    from fno.agents.registry import load_registry

    rows = load_registry()
    assert len(rows) == 1
    row = rows[0]
    assert row.mux == {"session": "main", "pane_id": 7}
    assert row.harness_session_id == result.session_uuid
    assert row.pid == 4242
    assert row.status == "live"
    # The row keeps short_id empty: it is the worker/bg transport slot, and a mux
    # row holds exactly one live ref (validate_single_live_ref).
    assert row.short_id == ""
    # The pane receipt carries the generated mailbox handle, not a provider
    # transport job id, so the caller can mail the pane from the receipt.
    assert result.short_id == result.session_uuid[:8]
    # That canonical handle resolves back to this exact row.
    from fno.agents.registry import resolve_agent_in

    resolved = resolve_agent_in(rows, result.short_id)
    assert resolved.entry.name == "peer"
    assert resolved.matched_by == "canonical_handle"


def test_ac1_hp_session_resolution_env_beats_default(
    tmp_path: Path, monkeypatch
) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.setenv("FNO_SESSION", "work")
    from fno.agents.mux_spawn import dispatch_spawn_pane

    runner = FakeRunner()
    result = dispatch_spawn_pane(
        name="peer", message="", provider="claude", cwd=tmp_path, runner=runner
    )
    assert result.session == "work"
    run_call = runner.calls[0]
    assert run_call[run_call.index("--session") + 1] == "work"
    # An explicit session beats the env.
    runner2 = FakeRunner()
    result2 = dispatch_spawn_pane(
        name="peer2",
        message="",
        provider="claude",
        cwd=tmp_path,
        session="other",
        runner=runner2,
    )
    assert result2.session == "other"


def test_ac1_err_mux_failure_leaves_no_row_and_names_session(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.dispatch import DispatchAskError

    with pytest.raises(DispatchAskError) as exc_info:
        _spawn(
            monkeypatch,
            tmp_path,
            runner=FakeRunner(run_returncode=1, run_stdout="", run_stderr="no pty"),
        )
    msg = str(exc_info.value)
    assert "'main'" in msg, f"error must name the mux session: {msg}"
    assert "no pty" in msg
    assert "fallback" in msg  # explicitly no daemon-PTY fallback

    from fno.agents.registry import load_registry

    assert load_registry() == [], "a failed spawn must not leave a half-created row"


def test_collision_refused_before_any_pane_spawn(tmp_path: Path, monkeypatch) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.dispatch import DispatchAskError
    from fno.agents.mux_spawn import dispatch_spawn_pane
    from fno.agents.registry import AgentEntry, write_registry

    write_registry(
        [AgentEntry(name="peer", harness="claude", cwd="/p", log_path="/l")]
    )
    runner = FakeRunner()
    with pytest.raises(DispatchAskError, match="already exists") as exc_info:
        dispatch_spawn_pane(
            name="peer", message="", provider="claude", cwd=tmp_path, runner=runner
        )
    assert exc_info.value.exit_code == 2
    assert runner.calls == [], "collision must refuse before any mux subprocess"


def test_ac1_fr_billing_guard_refuses_print_argv_before_pane(
    tmp_path: Path, monkeypatch
) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    import fno.agents.mux_spawn as mux_spawn

    # The builder never emits -p by construction; force it to prove the guard
    # sits between argv resolution and the pane spawn.
    monkeypatch.setattr(
        mux_spawn, "build_pane_argv", lambda *a, **k: ["claude", "-p", "hi"]
    )
    runner = FakeRunner()
    from fno.agents.dispatch import DispatchAskError

    with pytest.raises(DispatchAskError, match="-p/--print"):
        mux_spawn.dispatch_spawn_pane(
            name="peer", message="hi", provider="claude", cwd=tmp_path, runner=runner
        )
    assert runner.calls == [], "the guard must fire BEFORE any pane exists"

    # The predicate itself, both spellings.
    assert not mux_spawn.claude_argv_is_interactive(["claude", "-p"])
    assert not mux_spawn.claude_argv_is_interactive(["claude", "--print", "x"])
    assert mux_spawn.claude_argv_is_interactive(["claude", "--session-id", "u", "msg"])


def test_build_pane_argv_provider_forms(tmp_path: Path) -> None:
    from fno.agents.mux_spawn import build_pane_argv

    claude = build_pane_argv("claude", "task", tmp_path, False, "uuid-1")
    assert claude == ["claude", "--session-id", "uuid-1", "task"]

    codex = build_pane_argv("codex", "task", tmp_path, False, None)
    assert codex[:3] == ["codex", "-C", str(tmp_path)]
    assert "--sandbox" in codex and codex[-1] == "task"
    codex_yolo = build_pane_argv("codex", "", tmp_path, True, None)
    assert "--dangerously-bypass-approvals-and-sandbox" in codex_yolo

    gemini = build_pane_argv("gemini", "task", tmp_path, False, None)
    assert gemini[:2] == ["gemini", "--skip-trust"]
    assert "-i" in gemini
    # Bare interactive session: no -i without a message.
    assert "-i" not in build_pane_argv("gemini", "", tmp_path, False, None)

    # x-8f7f US1: agy is never-prompt, stateless (no --session-id), message as
    # trailing positional; never `-p` (that is agy's headless/print form).
    agy = build_pane_argv("agy", "task", tmp_path, False, "ignored-uuid")
    assert agy == ["agy", "--dangerously-skip-permissions", "task"]
    assert "-p" not in agy and "--session-id" not in agy
    assert build_pane_argv("agy", "", tmp_path, False, None) == [
        "agy",
        "--dangerously-skip-permissions",
    ]

    # x-51f6 US2: bare `opencode` is the TUI; the message rides --prompt (the
    # positional is a PROJECT PATH, not a prompt), --auto only under yolo,
    # and never the headless `run` subcommand.
    # x-c772: opencode is always launched with a model (the z-ai/glm-5.2 default).
    opencode = build_pane_argv("opencode", "task", tmp_path, False, "ignored")
    assert opencode == ["opencode", "--prompt", "task", "--model", "z-ai/glm-5.2"]
    assert build_pane_argv("opencode", "", tmp_path, False, None) == [
        "opencode",
        "--model",
        "z-ai/glm-5.2",
    ]
    opencode_yolo = build_pane_argv("opencode", "task", tmp_path, True, None)
    assert opencode_yolo == [
        "opencode",
        "--prompt",
        "task",
        "--model",
        "z-ai/glm-5.2",
        "--auto",
    ]
    assert "run" not in opencode and "--session-id" not in opencode


def test_build_pane_argv_normalizes_direct_slash_commands(tmp_path: Path) -> None:
    """Direct ``fno agents spawn`` calls bypass resolve_dispatch.

    The pane argv builder is therefore a load-bearing normalization choke point,
    including for the advertised plugin-qualified command spelling.
    """
    from fno.agents.mux_spawn import build_pane_argv

    assert build_pane_argv(
        "codex", "/fno:target x-81ad", tmp_path, False, None
    )[-1] == "$fno:target x-81ad"
    assert build_pane_argv(
        "opencode", "/fno:target x-81ad", tmp_path, False, None
    )[2] == "/fno:target x-81ad"
    assert build_pane_argv(
        "claude", "/fno:target x-81ad", tmp_path, False, "uuid"
    )[-1] == "/fno:target x-81ad"


def test_gemini_direct_slash_spawn_refuses_cleanly(tmp_path: Path, monkeypatch) -> None:
    """Deprecated harness refusal stays inside the public dispatch error type."""
    from fno.agents.dispatch import DispatchAskError

    with pytest.raises(DispatchAskError, match="successor 'agy'") as exc_info:
        _spawn(
            monkeypatch,
            tmp_path,
            provider="gemini",
            message="/fno:target x-81ad",
        )
    assert exc_info.value.exit_code == 2


def test_build_pane_argv_forwards_model(tmp_path: Path) -> None:
    # x-c772: an explicit --model reaches every pane provider's TUI flag
    # (opencode included, now that it is spawnable). Exact passthrough; opencode
    # uses the provider/model form and always carries a model (z-ai/glm-5.2 default).
    from fno.agents.mux_spawn import _PER_HARNESS_DEFAULT_MODEL, build_pane_argv

    cases = [
        ("claude", "u", "opus"),
        ("codex", None, "gpt-5.5"),
        ("gemini", None, "gemini-3-pro"),
        ("agy", None, "some-model"),
        ("opencode", None, "anthropic/claude-opus-4-8"),
    ]
    for provider, sid, model in cases:
        argv = build_pane_argv(provider, "t", tmp_path, False, sid, model)
        assert argv[argv.index("--model") + 1] == model, provider

    # claude/codex/gemini/agy: None/empty model -> no --model flag.
    for p in ("claude", "codex", "gemini", "agy"):
        assert "--model" not in build_pane_argv(p, "t", tmp_path, False, None, None)
        assert "--model" not in build_pane_argv(p, "t", tmp_path, False, None, "")

    # opencode ALWAYS carries a model: None/empty falls back to the default.
    for m in (None, ""):
        argv = build_pane_argv("opencode", "t", tmp_path, False, None, m)
        assert argv[argv.index("--model") + 1] == _PER_HARNESS_DEFAULT_MODEL["opencode"]


def test_opencode_default_is_a_table_lookup(tmp_path: Path, monkeypatch) -> None:
    # AC7-EDGE: opencode's default reads from the provider-keyed table, not a
    # hardcoded branch. Retargeting the entry retargets the injected argv;
    # removing it injects no --model at all; an explicit --model still wins.
    import fno.agents.mux_spawn as ms
    from fno.agents.mux_spawn import build_pane_argv

    # retarget: a sentinel entry flows straight through to argv
    monkeypatch.setattr(ms, "_PER_HARNESS_DEFAULT_MODEL", {"opencode": "sentinel/x"})
    argv = build_pane_argv("opencode", "t", tmp_path, False, None, None)
    assert argv[argv.index("--model") + 1] == "sentinel/x"

    # remove the entry: no --model injected (a hardcoded branch would still add one)
    monkeypatch.setattr(ms, "_PER_HARNESS_DEFAULT_MODEL", {})
    assert "--model" not in build_pane_argv("opencode", "t", tmp_path, False, None, None)

    # explicit --model overrides the (now empty) table
    argv = build_pane_argv("opencode", "t", tmp_path, False, None, "mine/y")
    assert argv[argv.index("--model") + 1] == "mine/y"


def test_build_pane_argv_forwards_tier3_flags(tmp_path: Path) -> None:
    # x-b6e2: --add-dir/--agent/--tools/--deny-tools map to claude's own
    # spellings in a fixed order; codex/agy map only --add-dir; opencode maps
    # only --agent. Fixed order enforces the Rust/Python parity contract.
    from fno.agents.mux_spawn import build_pane_argv

    claude = build_pane_argv(
        "claude", "t", tmp_path, False, "u",
        add_dir="/work", agent="reviewer", tools="Read,Edit", deny_tools="Bash",
    )
    # tokens present, in order.
    for a, b in [("--add-dir", "/work"), ("--agent", "reviewer"),
                 ("--allowedTools", "Read,Edit"), ("--disallowedTools", "Bash")]:
        assert claude[claude.index(a) + 1] == b
    assert claude.index("--add-dir") < claude.index("--agent") < \
        claude.index("--allowedTools") < claude.index("--disallowedTools")

    codex = build_pane_argv("codex", "t", tmp_path, False, None, add_dir="/extra")
    assert codex[codex.index("--add-dir") + 1] == "/extra"
    agy = build_pane_argv("agy", "t", tmp_path, False, None, add_dir="/extra")
    assert agy[agy.index("--add-dir") + 1] == "/extra"
    opencode = build_pane_argv("opencode", "t", tmp_path, False, None, agent="build")
    assert opencode[opencode.index("--agent") + 1] == "build"


def _pane_repo(tmp_path: Path) -> Path:
    import subprocess

    repo = tmp_path / "repo"
    repo.mkdir()
    subprocess.run(["git", "init", "-q"], cwd=repo, check=True)
    return repo


def test_build_pane_argv_codex_grants_git_metadata_write(tmp_path: Path) -> None:
    """AC4-HP: a sandboxed codex pane in a repo carries --add-dir <.git>.

    Same trap as the headless lane: workspace-write makes .git read-only, so
    without the grant the pane worker cannot commit at all.
    """
    from fno.agents.mux_spawn import build_pane_argv

    repo = _pane_repo(tmp_path)
    argv = build_pane_argv("codex", "t", repo, False, None)

    assert "--add-dir" in argv
    assert Path(argv[argv.index("--add-dir") + 1]).resolve() == (repo / ".git").resolve()


def test_build_pane_argv_codex_git_grant_tracks_resolved_posture(tmp_path: Path) -> None:
    """AC5-EDGE: the grant follows whether the pane is actually sandboxed.

    Only the two unsandboxed postures skip it. --full-auto and any
    <sandbox>:<approval> form are still sandboxed and still need it.
    """
    from fno.agents.mux_spawn import build_pane_argv

    repo = _pane_repo(tmp_path)

    assert "--add-dir" not in build_pane_argv("codex", "t", repo, True, None)
    assert "--add-dir" not in build_pane_argv(
        "codex", "t", repo, False, None, permission_mode="yolo"
    )
    assert "--add-dir" in build_pane_argv(
        "codex", "t", repo, False, None, permission_mode="full-auto"
    )
    assert "--add-dir" in build_pane_argv(
        "codex", "t", repo, False, None, permission_mode="workspace-write:on-request"
    )


def test_build_pane_argv_codex_git_grant_composes_with_user_add_dir(tmp_path: Path) -> None:
    """AC-EDGE: --add-dir is repeatable; a caller's own grant survives."""
    from fno.agents.mux_spawn import build_pane_argv

    repo = _pane_repo(tmp_path)
    argv = build_pane_argv("codex", "t", repo, False, None, add_dir="/extra")

    assert argv.count("--add-dir") == 2
    assert "/extra" in argv


def test_build_pane_argv_tier3_fails_closed(tmp_path: Path) -> None:
    # x-b6e2: a no-equivalent (provider, flag) cell raises BEFORE any spawn.
    from fno.agents.dispatch import DispatchAskError
    from fno.agents.mux_spawn import build_pane_argv

    closed = [
        ("codex", {"agent": "x"}),
        ("codex", {"tools": "Read"}),
        ("agy", {"deny_tools": "Bash"}),
        ("opencode", {"add_dir": "/w"}),
        ("gemini", {"add_dir": "/w"}),
        ("gemini", {"agent": "x"}),
    ]
    for provider, kw in closed:
        with pytest.raises(DispatchAskError):
            build_pane_argv(provider, "t", tmp_path, False, None, **kw)

    # AC2-ERR: an EMPTY value is unset, not a stray token - it must NOT trip the
    # fail-closed guard even on a no-equivalent provider (gemini review finding).
    for provider, kw in closed:
        argv = build_pane_argv(provider, "t", tmp_path, False, None, **{k: "" for k in kw})
        assert "--add-dir" not in argv and "--agent" not in argv


def test_pane_hostable_set_stays_in_sync_with_build_pane_argv(tmp_path: Path) -> None:
    """x-8f7f: PANE_HOSTABLE_PROVIDERS is the pane gate's source of truth and MUST
    match build_pane_argv's branches exactly - every listed provider builds argv,
    and a readable-but-argvless provider (opencode, staged inert until x-51f6) does
    NOT. This is the enforcement the borrowed READABLE_PROVIDERS list lacked."""
    from fno.agents.dispatch import DispatchAskError
    from fno.agents.mux_spawn import PANE_HOSTABLE_PROVIDERS, build_pane_argv
    from fno.agents.providers import READABLE_PROVIDERS

    for provider in PANE_HOSTABLE_PROVIDERS:
        argv = build_pane_argv(provider, "", tmp_path, False, None)
        assert argv and argv[0] == provider

    # opencode graduated from readable-but-argvless to pane-hostable (x-51f6);
    # the two sets coincide again until the next staged provider. Any future
    # readable-but-argvless provider must keep raising at build_pane_argv.
    assert "opencode" in PANE_HOSTABLE_PROVIDERS
    for readable in READABLE_PROVIDERS:
        if readable not in PANE_HOSTABLE_PROVIDERS:
            with pytest.raises(DispatchAskError, match="no interactive pane form"):
                build_pane_argv(readable, "", tmp_path, False, None)


def test_ac1_host_pane_gate_admits_hosted_rejects_unhosted(
    tmp_path: Path, monkeypatch
) -> None:
    """x-8f7f US1/US3 (+ x-51f6): the pane gate is PANE_HOSTABLE_PROVIDERS.
    agy and opencode (pane-hostable) are admitted and produce mux-hosted rows;
    a genuinely-unhosted CLI is rejected at the gate before any subprocess."""
    from fno.agents.dispatch import DispatchAskError

    # agy spawns a real (faked) mux pane -> a row lands.
    result, runner = _spawn(monkeypatch, tmp_path, provider="agy")
    assert result.provider == "agy"
    assert runner.calls[0][1:4] == ["mux", "pane", "run"]

    # opencode is pane-hostable since x-51f6 -> a row lands too.
    oc_result, oc_runner = _spawn(monkeypatch, tmp_path, provider="opencode", name="oc")
    assert oc_result.provider == "opencode"
    assert oc_runner.calls[0][1:4] == ["mux", "pane", "run"]

    # Registry-state assertion (not just the mocked call shape): a well-formed
    # row actually landed for both, mirroring the rigor of
    # test_ac1_hp_spawn_pane_runs_mux_and_writes_mux_ref_row's claude checks.
    from fno.agents.registry import load_registry

    rows = {row.name: row for row in load_registry()}
    assert set(rows) == {"peer", "oc"}
    agy_row = rows["peer"]
    assert agy_row.harness == "agy"
    assert agy_row.mux == {"session": "main", "pane_id": 7}  # FakeRunner default
    assert agy_row.status == "live"
    oc_row = rows["oc"]
    assert oc_row.harness == "opencode"
    assert oc_row.mux == {"session": "main", "pane_id": 7}
    assert oc_row.status == "live"

    # aider is not pane-hostable -> refused before any mux subprocess.
    with pytest.raises(DispatchAskError, match="unknown provider 'aider'"):
        _spawn(monkeypatch, tmp_path, provider="aider", name="ai")


def test_unparseable_pane_id_is_a_loud_error(tmp_path: Path, monkeypatch) -> None:
    from fno.agents.dispatch import DispatchAskError

    with pytest.raises(DispatchAskError, match="unparseable"):
        _spawn(
            monkeypatch,
            tmp_path,
            runner=FakeRunner(run_stdout="not-a-pane-id\n"),
        )
    from fno.agents.registry import load_registry

    assert load_registry() == []


def test_child_pid_lookup_is_best_effort(tmp_path: Path, monkeypatch) -> None:
    # A broken `pane ls` must not fail the spawn: pid stays None.
    result, _ = _spawn(
        monkeypatch, tmp_path, runner=FakeRunner(ls_stdout="not json")
    )
    assert result.child_pid is None
    from fno.agents.registry import load_registry

    assert load_registry()[0].pid is None


def test_routing_pane_substrate_spawn_stays_python() -> None:
    """4a-G2 routing carve-out: a pane spawn (explicit or default) never
    auto-routes to the Rust client; bg/headless spawns still do."""
    from fno.agents.rust_runtime import _is_pane_substrate_spawn

    assert _is_pane_substrate_spawn("spawn", ["spawn", "peer"])
    assert _is_pane_substrate_spawn("spawn", ["spawn", "peer", "--substrate", "pane"])
    assert _is_pane_substrate_spawn("spawn", ["spawn", "peer", "--substrate=pane"])
    assert not _is_pane_substrate_spawn("spawn", ["spawn", "p", "--substrate", "bg"])
    assert not _is_pane_substrate_spawn(
        "spawn", ["spawn", "p", "--substrate=headless"]
    )
    # x-c772: --headless / --once is the headless shortcut -> never a pane. `-H`
    # was reassigned to --harness (x-6de8), so `-H codex` is a default-pane spawn.
    assert not _is_pane_substrate_spawn("spawn", ["spawn", "p", "--headless"])
    assert not _is_pane_substrate_spawn("spawn", ["spawn", "p", "--once"])
    assert _is_pane_substrate_spawn("spawn", ["spawn", "p", "-H", "codex"])
    assert not _is_pane_substrate_spawn("ask", ["ask", "peer", "hi"])
    # The scan stops at --argv: payload tokens cannot masquerade as our flag.
    assert _is_pane_substrate_spawn(
        "spawn", ["spawn", "p", "--argv", "--substrate", "bg"]
    )


def test_routing_provenance_bearing_spawn_stays_python() -> None:
    """x-84a8: a spawn carrying --node/--slug/--plan is Python-only (the Rust
    client cannot parse them), even on a bg substrate that would otherwise route
    to the binary. Covers the /agent spawn.sh forward AND a direct CLI call."""
    from fno.agents.rust_runtime import _is_provenance_bearing_spawn

    assert _is_provenance_bearing_spawn("spawn", ["spawn", "p", "--node", "x-84a8"])
    assert _is_provenance_bearing_spawn("spawn", ["spawn", "p", "--node=x-84a8"])
    assert _is_provenance_bearing_spawn(
        "spawn", ["spawn", "p", "--substrate", "bg", "--slug", "s"]
    )
    assert _is_provenance_bearing_spawn("spawn", ["spawn", "p", "--plan", "a.md"])
    assert not _is_provenance_bearing_spawn("spawn", ["spawn", "p"])
    assert not _is_provenance_bearing_spawn("ask", ["ask", "p", "--node", "x"])


def test_provenance_vars_ride_wrapper_for_node_driven(
    tmp_path: Path, monkeypatch
) -> None:
    """x-84a8 AC(happy): a node-driven pane spawn exports FNO_NODE/SLUG/PLAN
    into the pane env alongside the mesh identity."""
    _, runner = _spawn(
        monkeypatch,
        tmp_path,
        provenance={"FNO_NODE": "x-84a8", "FNO_SLUG": "pane-prov", "FNO_PLAN": "p.md"},
    )
    tail = runner.calls[0][runner.calls[0].index("--") + 1 :]
    assert "FNO_NODE=x-84a8" in tail
    assert "FNO_SLUG=pane-prov" in tail
    assert "FNO_PLAN=p.md" in tail


def test_ad_hoc_spawn_exports_no_provenance(tmp_path: Path, monkeypatch) -> None:
    """x-84a8 AC(edge): an ad-hoc spawn (no node) exports no FNO_NODE/SLUG/PLAN,
    and no empty-string variants.

    Asserted against ASSIGNMENTS (`KEY=`), not bare tokens: the wrapper now also
    emits `-u KEY` to clear an inherited value, whose operand is the bare key.
    Unsetting satisfies this AC more strongly than omitting would.
    """
    _, runner = _spawn(monkeypatch, tmp_path)  # default: no provenance
    tail = runner.calls[0][runner.calls[0].index("--") + 1 :]
    assert not any(
        t.startswith(("FNO_NODE=", "FNO_SLUG=", "FNO_PLAN=")) for t in tail
    )


def test_mesh_env_wrapper_scrubs_inherited_session_identity() -> None:
    """AC6-HP: a spawned child inherits its parent's ROUTE but never its
    parent's IDENTITY. An ambient session marker riding through this seam is
    exactly how a claude worker spawned from a codex parent comes to carry a
    foreign CODEX_THREAD_ID and resolves as the wrong harness. Each harness
    re-mints its own marker for the child, so scrubbing the inherited set is
    lossless.

    Every name in AMBIENT_IDENTITY_ENV must be unset (the direct-read legacy
    markers like CLAUDECODE_SESSION_ID are included, not just the resolver
    tuple), while a routing var survives untouched.
    """
    from fno.agents.mux_spawn import _mesh_env_wrapper
    from fno.harness_identity import AMBIENT_IDENTITY_ENV

    wrapper = _mesh_env_wrapper(
        "child-1",
        "claude",
        role=None,
        argv=["claude", "--print", "hi"],
        account_env={"CLAUDE_CONFIG_DIR": "/acct"},
        route_env={"ANTHROPIC_API_KEY": "sk-route"},
    )
    assert wrapper[0] == "env"
    unset_names = {
        wrapper[i + 1] for i, token in enumerate(wrapper) if token == "-u"
    }
    # Every ambient identity name is unset, whatever the harness family.
    for name in AMBIENT_IDENTITY_ENV:
        assert name in unset_names, f"identity marker {name} not scrubbed"

    # No identity name is re-assigned to the child.
    assignments = {
        token.split("=", 1)[0]
        for token in wrapper
        if isinstance(token, str) and "=" in token and not token.startswith("-")
    }
    for name in AMBIENT_IDENTITY_ENV:
        assert name not in assignments, f"identity marker {name} re-exported"

    # Routing survives the scrub: route and account vars are untouched.
    assert "ANTHROPIC_API_KEY=sk-route" in wrapper
    assert "CLAUDE_CONFIG_DIR=/acct" in wrapper


def test_mesh_env_wrapper_scrubs_identity_for_every_provider() -> None:
    """AC6-HP: the scrub is not claude-specific. A codex pane spawned from a
    claude parent sheds CLAUDE_CODE_SESSION_ID the same way; identity is
    per-process, never inherited, for every harness family."""
    from fno.agents.mux_spawn import _mesh_env_wrapper

    for provider in ("codex", "gemini", "opencode", "agy", "claude"):
        wrapper = _mesh_env_wrapper(
            "child", provider, role=None, argv=["shell", "-c", "true"]
        )
        unset_names = {
            wrapper[i + 1] for i, token in enumerate(wrapper) if token == "-u"
        }
        assert "CLAUDE_CODE_SESSION_ID" in unset_names
        assert "CODEX_THREAD_ID" in unset_names


def test_resolve_provenance_branches(tmp_path: Path, monkeypatch) -> None:
    """resolve_provenance: explicit slug/plan skip the graph read; a linked plan
    yields FNO_PLAN, an empty one drops it; no node -> {}."""
    use_tmpdir(monkeypatch, tmp_path)  # empty graph, so any read misses
    from fno.agents.mux_spawn import resolve_provenance

    # No node -> nothing (the ad-hoc edge case at the resolver level).
    assert resolve_provenance(None) == {}

    # Explicit slug+plan: no graph needed, all three present.
    assert resolve_provenance("x-1", "the-slug", "plan.md") == {
        "FNO_NODE": "x-1",
        "FNO_SLUG": "the-slug",
        "FNO_PLAN": "plan.md",
    }

    # An unlinked plan (empty string) is dropped, slug kept.
    assert resolve_provenance("x-2", "s2", "") == {"FNO_NODE": "x-2", "FNO_SLUG": "s2"}

    # Unknown node + empty graph degrades to the node id alone (no raise).
    assert resolve_provenance("x-missing") == {"FNO_NODE": "x-missing"}


def test_cmd_spawn_node_flag_resolves_and_passes_provenance(
    tmp_path: Path, monkeypatch
) -> None:
    """x-84a8: `fno agents spawn --node ... --slug ... --plan ...` resolves the
    provenance map and hands it to dispatch_spawn_pane."""
    from typer.testing import CliRunner

    import fno.agents.cli as agents_cli
    import fno.agents.mux_spawn as mux_spawn
    from fno.agents.mux_spawn import MuxSpawnResult

    captured: dict = {}

    def fake_dispatch(**kwargs):
        captured.update(kwargs)
        return MuxSpawnResult(
            name=kwargs["name"], provider=kwargs["provider"], session="main",
            pane_id=1, child_pid=None, session_uuid="u",
        )

    monkeypatch.setattr(mux_spawn, "dispatch_spawn_pane", fake_dispatch)
    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")

    res = CliRunner().invoke(
        agents_cli.agents_app,
        ["spawn", "peer", "--harness", "claude",
         "--node", "x-84a8", "--slug", "s", "--plan", "p.md"],
    )
    assert res.exit_code == 0, res.output
    assert captured["provenance"] == {
        "FNO_NODE": "x-84a8", "FNO_SLUG": "s", "FNO_PLAN": "p.md",
    }


def test_cmd_spawn_pane_receipt_shape(tmp_path: Path, monkeypatch) -> None:
    """The public CLI launches and reports the same translated pane payload."""
    from typer.testing import CliRunner

    import fno.agents.cli as agents_cli
    import fno.agents.mux_spawn as mux_spawn

    use_tmpdir(monkeypatch, tmp_path)
    fake_runner = FakeRunner(run_stdout="9\n")
    real_dispatch = mux_spawn.dispatch_spawn_pane

    def dispatch_with_fake_mux(**kwargs):
        return real_dispatch(**kwargs, runner=fake_runner)

    monkeypatch.setattr(mux_spawn, "dispatch_spawn_pane", dispatch_with_fake_mux)
    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    monkeypatch.setenv("FNO_SESSION", "main")
    # x-85fe: pin canonical == caller so this node-less spawn does NOT move to
    # the canonical root (AC1-EDGE no-op) -- the receipt/redirect note would
    # otherwise drift when run from a linked worktree. This test checks the pane
    # receipt shape, not the cwd move.
    monkeypatch.setenv("FNO_REPO_ROOT", os.getcwd())

    runner = CliRunner()
    result = runner.invoke(
        agents_cli.agents_app,
        ["spawn", "--name", "peer", "--harness", "codex", "/fno:target x-81ad"],
    )
    assert result.exit_code == 0, result.output
    receipt = json.loads(result.output.strip().splitlines()[-1])
    assert receipt == {
        "name": "peer",
        "short_id": "",
        "harness": "codex",
        "harness_source": "explicit",  # dispatch-harness provenance
        "status": "spawning",
        "mux_session": "main",
        "pane_id": 9,
        "effective_message": "$fno:target x-81ad",
        # x-cdca: an unbound receipt says so, says whether the pane is still
        # there, and says why. Without these, this exact receipt shape - status
        # `spawning` with an empty short_id, exit 0 - was indistinguishable from
        # one whose pane had already died, and callers re-prompted the corpse.
        "bound": False,
        "pane_alive": None,
        "unbound_reason": "no-child-pid-to-correlate",
    }
    # The invariant that makes an empty short_id a signal rather than a
    # formatting detail (claude/codex, where short_id IS the handle).
    assert receipt["bound"] == bool(receipt["short_id"])
    # AC5: no -P/--route on this spawn -> provider (vendor) and model keys are
    # ABSENT, not defaulted to the harness. A provider key holding a harness
    # literal is the four-axis defect this receipt shape corrects.
    assert "provider" not in receipt
    assert "model" not in receipt
    pane_run = next(call for call in fake_runner.calls if call[1:4] == ["mux", "pane", "run"])
    assert "$fno:target x-81ad" in pane_run


def test_cmd_spawn_pane_bound_codex_receipt_carries_full_identity(
    tmp_path: Path, monkeypatch
) -> None:
    """AC1-HP: a bound public receipt exposes the canonical full thread ID."""
    from typer.testing import CliRunner

    import fno.agents.cli as agents_cli
    import fno.agents.mux_spawn as mux_spawn
    from fno.agents.mux_spawn import MuxSpawnResult

    use_tmpdir(monkeypatch, tmp_path)
    session_id = "019fb024-2327-75f3-8b80-06e9d5ade05f"
    monkeypatch.setattr(
        mux_spawn,
        "dispatch_spawn_pane",
        lambda **kwargs: MuxSpawnResult(
            name=kwargs["name"],
            provider="codex",
            session="main",
            pane_id=9,
            child_pid=4242,
            session_uuid=session_id,
            short_id=session_id[-8:],
            status="live",
        ),
    )
    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    monkeypatch.setenv("FNO_REPO_ROOT", os.getcwd())

    result = CliRunner().invoke(
        agents_cli.agents_app,
        ["spawn", "--name", "peer", "--harness", "codex", "hello"],
    )

    assert result.exit_code == 0, result.output
    receipt = json.loads(result.output.strip().splitlines()[-1])
    assert receipt["status"] == "live"
    assert receipt["session_id"] == session_id
    assert receipt["short_id"] == session_id[-8:]


def test_cmd_spawn_rejects_output_format_on_pane_before_dispatch(
    tmp_path: Path, monkeypatch
) -> None:
    from typer.testing import CliRunner

    import fno.agents.cli as agents_cli
    import fno.agents.mux_spawn as mux_spawn

    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    monkeypatch.setattr(
        mux_spawn,
        "dispatch_spawn_pane",
        lambda **_kwargs: pytest.fail("pane dispatch must not run"),
    )

    result = CliRunner().invoke(
        agents_cli.agents_app,
        [
            "spawn", "--name", "peer", "--harness", "claude",
            "--output-format", "json", "hello",
        ],
    )

    assert result.exit_code == 2, result.output
    assert "supports only 'json' on claude headless spawns" in result.output


def test_cmd_spawn_parses_pr_watch_headless_json_argv(
    tmp_path: Path, monkeypatch
) -> None:
    from typer.testing import CliRunner

    import fno.agents.cli as agents_cli
    import fno.agents.dispatch as dispatch_mod
    from fno.agents.dispatch import SpawnResult

    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    captured: dict = {}

    def fake_dispatch(**kwargs):
        captured.update(kwargs)
        return SpawnResult(
            kind="once",
            name=kwargs["name"],
            provider=kwargs["provider"],
            short_id="",
            reply='{"is_error": false}',
        )

    monkeypatch.setattr(dispatch_mod, "dispatch_spawn", fake_dispatch)
    result = CliRunner().invoke(
        agents_cli.agents_app,
        [
            "spawn", "--name", "pr-check-7", "--substrate", "headless",
            "--harness", "claude", "--output-format", "json",
            "/fno:pr check 7",
        ],
    )

    assert result.exit_code == 0, result.output
    assert captured["message"] == "/fno:pr check 7"
    assert captured["headless"] is True
    assert captured["output_format"] == "json"


# ---------------------------------------------------------------------------
# x-3e38 pane placement: squad/split on the outer pane-run transport
# ---------------------------------------------------------------------------


def test_placement_directives_ride_outer_pane_run_before_separator(
    tmp_path: Path, monkeypatch
) -> None:
    # AC1-HP/AC2-HP: placement rides the OUTER pane-run argv, before the `--`
    # fencing the provider argv; build_pane_argv stays placement-blind.
    _result, runner = _spawn(monkeypatch, tmp_path, squad="review", split="left")
    run_call = runner.calls[0]
    sep = run_call.index("--")
    outer = run_call[:sep]
    assert outer[outer.index("squad") + 1] == "review"
    assert outer[outer.index("split") + 1] == "left"
    tail = run_call[sep + 1 :]
    assert "squad" not in tail and "split" not in tail
    assert "claude" in tail  # provider argv unchanged


def test_placement_omitted_leaves_pane_run_argv_unchanged(
    tmp_path: Path, monkeypatch
) -> None:
    # AC4-EDGE: no placement -> exactly the current one-new-tab argv.
    _result, runner = _spawn(monkeypatch, tmp_path)
    run_call = runner.calls[0]
    sep = run_call.index("--")
    assert "squad" not in run_call[:sep] and "split" not in run_call[:sep]


def test_cmd_spawn_placement_rejected_on_bg_substrate(tmp_path: Path, monkeypatch) -> None:
    # AC4-ERR: pane geometry flags fail closed on a substrate with no pane tree,
    # before any spawn.
    from typer.testing import CliRunner

    import fno.agents.cli as agents_cli

    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    res = CliRunner().invoke(
        agents_cli.agents_app,
        ["spawn", "peer", "--harness", "claude", "--substrate", "bg", "-x", "left"],
    )
    assert res.exit_code == 2, res.output
    assert "--split/-x, and --at apply only to --substrate pane" in res.output


def test_cmd_spawn_rejects_bad_split_value(tmp_path: Path, monkeypatch) -> None:
    # AC4-ERR: an out-of-vocabulary direction is refused at the CLI boundary.
    from typer.testing import CliRunner

    import fno.agents.cli as agents_cli

    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    res = CliRunner().invoke(
        agents_cli.agents_app,
        ["spawn", "peer", "--harness", "claude", "-x", "diagonal"],
    )
    assert res.exit_code == 2, res.output
    assert "left, right, up, or down" in res.output


def test_cmd_spawn_rejects_blank_squad_before_dispatch(tmp_path: Path, monkeypatch) -> None:
    from typer.testing import CliRunner

    import fno.agents.cli as agents_cli

    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    res = CliRunner().invoke(
        agents_cli.agents_app,
        ["spawn", "peer", "--harness", "claude", "-s", ""],
    )
    assert res.exit_code == 2, res.output
    assert "--workspace/-s needs a nonblank workspace name" in res.output


@pytest.mark.parametrize(
    "placement_args",
    [
        ["--workspace", "review", "--split", "right"],
        ["--squad", "review", "--split", "right"],
        ["-s", "review", "-x", "right"],
    ],
)
def test_cmd_spawn_pane_threads_placement_to_dispatch(
    tmp_path: Path, monkeypatch, placement_args: list[str]
) -> None:
    # AC1-HP/AC2-HP: long and mobile aliases reach dispatch_spawn_pane.
    from typer.testing import CliRunner

    import fno.agents.cli as agents_cli
    import fno.agents.mux_spawn as mux_spawn
    from fno.agents.mux_spawn import MuxSpawnResult

    captured: dict = {}

    def fake_dispatch(**kwargs):
        captured.update(kwargs)
        return MuxSpawnResult(
            name=kwargs["name"],
            provider=kwargs["provider"],
            session="main",
            pane_id=1,
            child_pid=None,
            session_uuid="u",
        )

    monkeypatch.setattr(mux_spawn, "dispatch_spawn_pane", fake_dispatch)
    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    res = CliRunner().invoke(
        agents_cli.agents_app,
        ["spawn", "--name", "peer", "--harness", "claude", *placement_args],
    )
    assert res.exit_code == 0, res.output
    assert captured["squad"] == "review"
    assert captured["split"] == "right"


# ---------------------------------------------------------------------------
# _mesh_env_wrapper: a routed pane scrubs the parent's Anthropic creds (x-db50)
# ---------------------------------------------------------------------------


def test_mesh_env_wrapper_routed_pane_scrubs_anthropic_creds(monkeypatch):
    """A routed role must prefix `env -u ANTHROPIC_API_KEY -u
    CLAUDE_CODE_OAUTH_TOKEN` so a parent API key / subscription OAuth token
    cannot override the routed AUTH_TOKEN."""
    from fno.agents import mux_spawn
    from fno.agents import model_routing

    monkeypatch.setattr(
        model_routing,
        "resolve_route",
        lambda role, **kw: {
            "ANTHROPIC_BASE_URL": "https://api.z.ai/api/anthropic",
            "ANTHROPIC_AUTH_TOKEN": "zk",
            "ANTHROPIC_MODEL": "glm-5.2",
        },
    )
    wrapped = mux_spawn._mesh_env_wrapper("w", "claude", "coordinate", ["claude"])
    assert wrapped[0] == "env"
    # -u flags precede any KEY=VAL assignment (env parses options first).
    assert "-u" in wrapped
    ui = wrapped.index("-u")
    first_assign = next(i for i, t in enumerate(wrapped) if "=" in t)
    unset_region = wrapped[ui:first_assign]
    assert "ANTHROPIC_API_KEY" in unset_region
    assert "CLAUDE_CODE_OAUTH_TOKEN" in unset_region
    assert ui < first_assign  # unsets before assignments
    assert "ANTHROPIC_AUTH_TOKEN=zk" in wrapped


def test_mesh_env_wrapper_codex_route_preserves_anthropic_credentials():
    """An OpenAI route must not scrub unrelated inherited Claude auth."""
    from fno.agents import mux_spawn

    wrapped = mux_spawn._mesh_env_wrapper(
        "w",
        "codex",
        None,
        ["codex"],
        route_env={
            "OPENAI_BASE_URL": "https://example.test/v1",
            "OPENAI_API_KEY": "key",
        },
    )

    assert "ANTHROPIC_API_KEY" not in wrapped
    assert "CLAUDE_CODE_OAUTH_TOKEN" not in wrapped
    assert "OPENAI_BASE_URL=https://example.test/v1" in wrapped
    assert "OPENAI_API_KEY=key" in wrapped


def test_mesh_env_wrapper_unrouted_pane_adds_no_unset(monkeypatch):
    """No role -> no route -> no AUTH scrub.

    Narrowed from "no `-u` at all": the wrapper now always clears the
    provenance triple, so a pane cannot inherit its spawner's node. The claim
    this test exists to make is about credentials, which are still untouched.
    """
    from fno.agents import mux_spawn

    wrapped = mux_spawn._mesh_env_wrapper("w", "claude", None, ["claude"])
    assert "ANTHROPIC_API_KEY" not in wrapped
    assert "CLAUDE_CODE_OAUTH_TOKEN" not in wrapped
    assert wrapped[0] == "env"


def test_mesh_env_wrapper_role_without_key_adds_no_unset(monkeypatch):
    """A routed role that resolves to None (no key) must not scrub creds either.

    Narrowed alongside the unrouted case: the provenance clear is unconditional,
    the credential scrub is not.
    """
    from fno.agents import mux_spawn
    from fno.agents import model_routing

    monkeypatch.setattr(model_routing, "resolve_route", lambda role, **kw: None)
    wrapped = mux_spawn._mesh_env_wrapper("w", "claude", "coordinate", ["claude"])
    assert "ANTHROPIC_API_KEY" not in wrapped
    assert "CLAUDE_CODE_OAUTH_TOKEN" not in wrapped


def test_mesh_env_wrapper_clears_inherited_provenance(monkeypatch):
    """A pane must not inherit its spawner's node.

    dispatch_spawn_pane hands the ambient environment to the self-spawning mux
    process, so without an explicit clear an ad-hoc pane carries the server's
    FNO_NODE and a plan-less child carries its FNO_PLAN - which ambient origin
    capture then persists into every node that pane files.
    """
    from fno.agents import mux_spawn

    adhoc = mux_spawn._mesh_env_wrapper("w", "claude", None, ["claude"])
    for key in mux_spawn.PROVENANCE_KEYS:
        assert ["-u", key] == adhoc[adhoc.index(key) - 1 : adhoc.index(key) + 1]

    # A resolved key is set, not cleared; the unresolved rest are still cleared.
    bound = mux_spawn._mesh_env_wrapper(
        "w", "claude", None, ["claude"], provenance={"FNO_NODE": "x-aaaa"}
    )
    assert "FNO_NODE=x-aaaa" in bound
    assert "-u" in bound and "FNO_PLAN" in bound
    assert "FNO_NODE" not in bound[: bound.index("FNO_NODE=x-aaaa")]


# -- x-6928: agents spawn --at current + interactive-readiness gate ----------


def test_exact_at_current_forwards_token_runs_json_and_reads_receipt(
    tmp_path: Path, monkeypatch
) -> None:
    """AC1-HP / AC1-UI: --at current forwards the token before the `--` fence,
    requests --json, and reads the SERVER-authored placement receipt - never
    synthesized from the requested flags. A painted, alive pane writes the row.
    """
    from fno.agents.registry import load_registry

    placement = {
        "anchor": 4,
        "direction": "Down",
        "fallback": "refuse",
        "squad": 1,
        "tab": 10,
    }
    result, runner = _spawn(
        monkeypatch,
        tmp_path,
        split="down",
        at="current",
        runner=FakeRunner(placement=placement, read_stdout="claude\n"),
    )
    run_call = runner.calls[0]
    assert run_call[1:4] == ["mux", "pane", "run"]
    assert "at" in run_call and "current" in run_call, "token forwarded verbatim"
    assert "--json" in run_call, "exact placement requests the server receipt"
    assert run_call.index("at") < run_call.index("--"), "token rides before the argv fence"
    assert result.placement == placement, "receipt is server-authored, not synthesized"
    # The readiness gate probes the spawn's own session, not the default.
    wait_call = next(c for c in runner.calls if c[1:4] == ["mux", "pane", "wait"])
    assert "--session" in wait_call, "readiness probe targets the spawn's session"
    assert [r.name for r in load_registry()] == ["peer"], "a ready spawn writes the row"
    assert runner.kill_calls == [], "no reap on a successful readiness gate"


def test_exact_at_current_kills_pane_and_writes_no_row_on_early_exit(
    tmp_path: Path, monkeypatch
) -> None:
    """AC5-ERR: a provider that exits before readiness is reaped (pane kill),
    the split collapses via tree normalization, and NO registry row is written.
    """
    from fno.agents.mux_spawn import DispatchAskError, dispatch_spawn_pane
    from fno.agents.registry import load_registry

    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.delenv("FNO_SESSION", raising=False)
    runner = FakeRunner(placement={"anchor": 4}, wait_returncode=12)
    with pytest.raises(DispatchAskError):
        dispatch_spawn_pane(
            name="peer",
            message="hi",
            provider="claude",
            cwd=tmp_path,
            split="down",
            at="current",
            runner=runner,
        )
    assert len(runner.kill_calls) == 1, "the transaction-owned pane was reaped"
    kill = runner.kill_calls[0]
    assert kill[1:4] == ["mux", "pane", "kill"]
    assert "--session" in kill and "main" in kill, "cleanup targets the spawn's session"
    assert "7" in kill, "the placed pane id is reaped"
    assert load_registry() == [], "no registry row on launch failure"


@pytest.mark.parametrize("probe", ["wait", "read"])
def test_exact_readiness_probe_error_reaps_without_writing_row(
    tmp_path: Path, monkeypatch, probe: str
) -> None:
    from fno.agents.mux_spawn import DispatchAskError, dispatch_spawn_pane
    from fno.agents.registry import load_registry

    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.delenv("FNO_SESSION", raising=False)
    runner = FakeRunner(
        placement={"anchor": 4},
        wait_returncode=99 if probe == "wait" else 11,
        read_returncode=99 if probe == "read" else 0,
        read_stdout="painted despite error",
        read_stderr="mux unavailable",
    )

    with pytest.raises(DispatchAskError) as exc:
        dispatch_spawn_pane(
            name="peer",
            message="hi",
            provider="claude",
            cwd=tmp_path,
            split="down",
            at="current",
            runner=runner,
        )

    assert "readiness probe failed" in str(exc.value)
    assert runner.kill_calls
    assert load_registry() == []


@pytest.mark.parametrize("cleanup_failure", ["nonzero", "timeout"])
def test_exact_readiness_failure_never_claims_unconfirmed_reap(
    tmp_path: Path, monkeypatch, cleanup_failure: str
) -> None:
    from fno.agents.mux_spawn import DispatchAskError, dispatch_spawn_pane
    from fno.agents.registry import load_registry

    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.delenv("FNO_SESSION", raising=False)
    runner = FakeRunner(
        placement={"anchor": 4},
        wait_returncode=12,
        kill_returncode=1,
        kill_stderr="permission denied",
        kill_exception=(
            subprocess.TimeoutExpired(["fno", "mux", "pane", "kill"], 30)
            if cleanup_failure == "timeout"
            else None
        ),
    )

    with pytest.raises(DispatchAskError) as exc:
        dispatch_spawn_pane(
            name="peer",
            message="hi",
            provider="claude",
            cwd=tmp_path,
            split="down",
            at="current",
            runner=runner,
        )

    message = str(exc.value)
    assert "pane 7 may still exist" in message
    assert "session 'main'" in message
    assert "pane 7 reaped" not in message
    assert load_registry() == []


def test_spawn_stamps_process_incarnation_token(tmp_path: Path, monkeypatch) -> None:
    """The registry row binds the pane PID and its process incarnation."""
    from fno.agents import spawn_gate
    from fno.agents.registry import load_registry

    monkeypatch.setattr(spawn_gate, "_process_start_time", lambda _pid: 987654)
    _spawn(monkeypatch, tmp_path, provider="codex")

    row = load_registry()[0]
    assert row.pid == 4242
    assert row.pid_start_time == 987654


@pytest.mark.parametrize(
    "failure_kind",
    ["os-error", "value-error", "identity-collision"],
)
def test_registry_write_failure_reaps_exact_spawned_pane(
    tmp_path: Path, monkeypatch, failure_kind: str
) -> None:
    """A pane without its registry identity is rolled back before failure."""
    from fno.agents import mux_spawn
    from fno.agents.mux_spawn import DispatchAskError, dispatch_spawn_pane
    from fno.agents.registry import AgentResolutionError, load_registry

    failure: Exception
    if failure_kind == "os-error":
        failure = OSError("disk full")
    elif failure_kind == "value-error":
        failure = ValueError("invalid row")
    else:
        failure = AgentResolutionError("identity collision", ambiguous=True)

    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.delenv("FNO_SESSION", raising=False)
    runner = FakeRunner()
    monkeypatch.setattr(
        mux_spawn, "update_registry",
        lambda *_args, **_kwargs: (_ for _ in ()).throw(failure),
    )

    with pytest.raises(DispatchAskError, match=r"registry write failed.*pane 7 reaped"):
        dispatch_spawn_pane(
            name="peer", message="hi", provider="codex", cwd=tmp_path, runner=runner,
        )

    assert runner.kill_calls == [
        ["fno", "mux", "pane", "kill", "--session", "main", "7"]
    ]
    assert load_registry() == []


def test_exact_at_current_proceeds_live_when_unpainted(
    tmp_path: Path, monkeypatch
) -> None:
    """AC5-ERR complement: an ALIVE but unpainted child is LIVE, not failure -
    the gate never reaps a live process; the row is written."""
    from fno.agents.registry import load_registry

    _spawn(
        monkeypatch,
        tmp_path,
        split="down",
        at="current",
        runner=FakeRunner(placement={"anchor": 4}, wait_returncode=11, read_stdout=""),
    )
    assert [r.name for r in load_registry()] == ["peer"], "a live spawn still writes the row"


# What a LIVE zai route actually carries, not just its endpoint half: the two
# connection keys plus `resolve_route`'s ANTHROPIC_MODEL, the four tier maps
# from MODEL_ENV_KEYS (haiku separately remapped to the provider's cheaper
# model), and the 1M compact window a `[1m]` model needs.
#
# The fixture used to hold only the two connection keys, so every happy test
# proved the endpoint reaches the pane and proved nothing about the model. A
# change that filtered keys anywhere along the carry would have kept the suite
# green while a happy pane ran on whatever model the ambient environment
# resolved -- or, quieter still, on the right model with a 200K window.
_ROUTE = {
    "ANTHROPIC_BASE_URL": "https://api.z.ai/api/anthropic",
    "ANTHROPIC_AUTH_TOKEN": "zai-secret",
    "ANTHROPIC_MODEL": "glm-5.2[1m]",
    "ANTHROPIC_DEFAULT_OPUS_MODEL": "glm-5.2[1m]",
    "ANTHROPIC_DEFAULT_SONNET_MODEL": "glm-5.2[1m]",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL": "glm-4.5-air",
    "ANTHROPIC_DEFAULT_FABLE_MODEL": "glm-5.2[1m]",
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW": "1000000",
}


def test_cmd_spawn_explicit_happy_monitor_routes_zai_pane(
    tmp_path: Path, monkeypatch
) -> None:
    """The public flag reaches the existing safe happy launcher seam."""
    from typer.testing import CliRunner

    import fno.agents.mux_spawn as mux_spawn
    import fno.agents.spawn_gate as spawn_gate
    from fno.cli import app
    from fno.agents.model_routing import DEFAULT_ZAI_BASE_URL

    class Gate:
        def release(self) -> None:
            pass

    use_tmpdir(monkeypatch, tmp_path)
    fake_runner = FakeRunner()
    real_dispatch = mux_spawn.dispatch_spawn_pane

    def dispatch_with_fake_mux(**kwargs):
        return real_dispatch(**kwargs, runner=fake_runner)

    monkeypatch.setattr(mux_spawn, "dispatch_spawn_pane", dispatch_with_fake_mux)
    monkeypatch.setattr(spawn_gate, "run_gate", lambda *args, **kwargs: Gate())
    monkeypatch.setattr(mux_spawn.shutil, "which", lambda binary: "/usr/local/bin/happy")
    monkeypatch.setattr(
        mux_spawn,
        "happy_routed_panes_enabled",
        lambda: pytest.fail("explicit --monitor happy must not read the config default"),
    )
    # No real SessionStart hook fires under the fake runner; script the
    # registration the bounded wait looks for so the spawn succeeds.
    monkeypatch.setattr(
        mux_spawn, "_await_pane_registration", lambda name, mux, r, *a, **k: ("happy-sid", "")
    )
    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    monkeypatch.setenv("FNO_REPO_ROOT", os.getcwd())
    monkeypatch.setenv("ZAI_API_KEY", "zai-secret")

    result = CliRunner().invoke(
        app,
        [
            "agents",
            "spawn",
            "--name",
            "peer",
            "--harness",
            "claude",
            "--provider",
            "zai",
            "--model",
            "glm-5.2",
            "--monitor",
            "happy",
            "hello",
        ],
    )

    assert result.exit_code == 0, result.output
    argv = _pane_run_argv(fake_runner)
    assert argv[0] == "env", "the credential scrub must stay outermost"
    happy_argv = argv[argv.index("happy") :]
    pairs = [
        happy_argv[index + 1]
        for index, token in enumerate(happy_argv)
        if token == "--claude-env"
    ]
    assert f"ANTHROPIC_BASE_URL={DEFAULT_ZAI_BASE_URL}" in pairs
    assert "ANTHROPIC_MODEL=glm-5.2" in pairs
    # The credential rides the env(1) wrapper, never --claude-env: happy is a
    # long-lived parent whose argv is a world-readable `ps` token, while env(1)
    # execs and its assignments leave the process image.
    assert not any(p.startswith("ANTHROPIC_AUTH_TOKEN=") for p in pairs)
    assert "ANTHROPIC_AUTH_TOKEN=zai-secret" in argv[: argv.index("happy")]
    assert "--settings" not in argv


@pytest.mark.parametrize(
    "substrate_args",
    [
        ["--substrate", "bg"],
        ["--substrate", "headless"],
        ["--once"],
        ["--headless"],
    ],
)
def test_cmd_spawn_explicit_happy_monitor_refuses_non_pane_before_dispatch(
    tmp_path: Path, monkeypatch, substrate_args: list[str]
) -> None:
    from typer.testing import CliRunner

    import fno.agents.cli as agents_cli
    import fno.agents.dispatch as dispatch
    import fno.agents.mux_spawn as mux_spawn

    use_tmpdir(monkeypatch, tmp_path)
    calls: list[str] = []
    monkeypatch.setattr(
        mux_spawn,
        "dispatch_spawn_pane",
        lambda **kwargs: calls.append("pane") or pytest.fail("pane dispatch called"),
    )
    monkeypatch.setattr(
        dispatch,
        "dispatch_spawn",
        lambda **kwargs: calls.append("worker") or pytest.fail("worker dispatch called"),
    )
    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")

    result = CliRunner().invoke(
        agents_cli.agents_app,
        [
            "spawn",
            "--name",
            "peer",
            "--harness",
            "claude",
            "--monitor",
            "happy",
            *substrate_args,
            "hello",
        ],
    )

    assert result.exit_code == 2, result.output
    assert "pane-only" in result.output
    assert calls == []


@pytest.mark.parametrize(
    ("extra_args", "message"),
    [
        (["--harness", "codex", "--provider", "zai", "--model", "glm-5.2"], "claude"),
        (["--harness", "claude"], "zai"),
        (
            [
                "--harness",
                "claude",
                "--provider",
                "zai",
                "--model",
                "glm-5.2",
                "--monitor",
                "other",
            ],
            "happy",
        ),
    ],
)
def test_cmd_spawn_explicit_happy_monitor_refuses_incompatible_selection(
    tmp_path: Path, monkeypatch, extra_args: list[str], message: str
) -> None:
    from typer.testing import CliRunner

    import fno.agents.cli as agents_cli

    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    args = ["spawn", "--name", "peer", *extra_args]
    if "--monitor" not in extra_args:
        args += ["--monitor", "happy"]
    args.append("hello")
    result = CliRunner().invoke(agents_cli.agents_app, args)

    assert result.exit_code == 2, result.output
    assert message in result.output


def test_cmd_spawn_explicit_happy_monitor_refuses_separate_model_override(
    tmp_path: Path, monkeypatch
) -> None:
    from typer.testing import CliRunner

    import fno.agents.cli as agents_cli
    import fno.agents.mux_spawn as mux_spawn

    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.setenv("ZAI_API_KEY", "zai-secret")
    monkeypatch.setattr(
        mux_spawn,
        "dispatch_spawn_pane",
        lambda **kwargs: pytest.fail("pane dispatch called"),
    )
    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    result = CliRunner().invoke(
        agents_cli.agents_app,
        [
            "spawn",
            "--harness",
            "claude",
            "--route",
            "zai/glm-5.2",
            "--model",
            "claude-opus-4-1",
            "--monitor",
            "happy",
            "hello",
        ],
    )

    assert result.exit_code == 2, result.output
    assert "separate --model" in result.output


def _claude_env_pairs(argv: list[str]) -> dict:
    """The route as happy would receive it: every ``--claude-env KEY=VALUE``."""
    return dict(
        argv[i + 1].split("=", 1)
        for i, token in enumerate(argv)
        if token == "--claude-env"
    )


def _env1_assignments(wrapped: list[str]) -> dict:
    """The ``NAME=VALUE`` run of an ``env(1)`` wrapper, as the child receives it.

    Handles the exact shape ``_mesh_env_wrapper`` emits: a leading ``env``, a run
    of ``-u NAME`` pairs, then assignments, then the command. Deliberately NOT a
    full mirror of the mux server's ``env_assignments_start`` (server.rs), which
    also steps over ``--unset NAME``, other flags, and a ``--`` terminator. If
    the wrapper ever grows one of those, this stops at it and the union
    assertions fail loudly rather than silently skipping a key.
    """
    assert wrapped[0] == "env", wrapped[:3]
    pairs: dict[str, str] = {}
    i = 1
    while i < len(wrapped):
        tok = wrapped[i]
        if tok == "-u":
            i += 2
            continue
        if "=" in tok and not tok.startswith("-"):
            key, value = tok.split("=", 1)
            pairs[key] = value
            i += 1
            continue
        break
    return pairs


def _wrapped_happy_argv(route: dict) -> list[str]:
    """The argv that actually executes for a happy pane: env(1) -> happy -> claude."""
    import fno.agents.mux_spawn as mux_spawn

    inner = mux_spawn.happy_pane_argv(["claude", "--model", "glm-5.2", "go"], route)
    return mux_spawn._mesh_env_wrapper("wk", "claude", None, inner, None, None, route)


def test_happy_pane_wrapped_argv_holds_the_union_invariant(monkeypatch) -> None:
    """Every route key reaches the child, and no credential reaches ``ps``.

    Asserted on the FULLY WRAPPED argv, not on happy_pane_argv's output, and
    that is the whole point of the test. happy builds its child env as
    ``{...process.env, ...claudeEnvVars}``, so a key is delivered whether it
    rides the env(1) run or --claude-env -- but only --claude-env publishes it
    to `ps`. A test that saw happy_pane_argv alone could not distinguish "the
    secret moved to the wrapper" (correct) from "the secret was dropped"
    (a 401 in production, and silent here).
    """
    from fno.agents.account_env import SECRET_ROUTE_VARS
    import fno.agents.mux_spawn as mux_spawn

    monkeypatch.setattr(mux_spawn.shutil, "which", lambda b: "/opt/homebrew/bin/happy")
    wrapped = _wrapped_happy_argv(_ROUTE)

    env1 = _env1_assignments(wrapped)
    claude_env = _claude_env_pairs(wrapped)
    secrets = [k for k in _ROUTE if k in SECRET_ROUTE_VARS]
    assert secrets, "fixture must carry a credential or this proves nothing"

    for key, value in _ROUTE.items():
        assert env1.get(key) == value or claude_env.get(key) == value, (
            f"{key} reaches the child on neither channel"
        )
    for key in secrets:
        assert key not in claude_env, f"{key} is a world-readable ps token"
        assert env1.get(key) == _ROUTE[key], f"{key} dropped, not moved"
    assert set(claude_env) == set(_ROUTE) - set(secrets), "none dropped, none invented"

    assert "--settings" not in wrapped
    assert wrapped[wrapped.index("happy") - 1] != "env", "env(1) must set, not just exec"
    assert wrapped[-3:] == ["--model", "glm-5.2", "go"]


def test_happy_pane_argv_carries_a_route_with_no_model_keys(monkeypatch) -> None:
    """A route legitimately carrying only an endpoint and a credential passes.

    The endpoint still rides --claude-env; the credential rides the wrapper
    only. A provider with no haiku_model and a model with no [1m] suffix are not
    held to the seven-key shape a full zai route happens to have.
    """
    import fno.agents.mux_spawn as mux_spawn

    minimal = {
        "ANTHROPIC_BASE_URL": "https://api.example.test/anthropic",
        "ANTHROPIC_AUTH_TOKEN": "other-secret",
    }
    monkeypatch.setattr(mux_spawn.shutil, "which", lambda b: "/opt/homebrew/bin/happy")
    wrapped = _wrapped_happy_argv(minimal)

    assert _claude_env_pairs(wrapped) == {
        "ANTHROPIC_BASE_URL": "https://api.example.test/anthropic"
    }
    assert _env1_assignments(wrapped)["ANTHROPIC_AUTH_TOKEN"] == "other-secret"


def test_happy_pane_argv_emits_no_claude_env_for_a_credential_only_route(
    monkeypatch,
) -> None:
    """An all-secret route yields NO --claude-env tokens, not a dangling flag.

    The filter drops every key here, so the boundary worth pinning is that the
    list collapses to nothing rather than to a `--claude-env` with no argument
    (which happy would parse as the next argv token, i.e. `claude`).
    """
    import fno.agents.mux_spawn as mux_spawn

    monkeypatch.setattr(mux_spawn.shutil, "which", lambda b: "/opt/homebrew/bin/happy")
    argv = mux_spawn.happy_pane_argv(["claude", "go"], {"ANTHROPIC_AUTH_TOKEN": "s"})

    assert "--claude-env" not in argv
    assert argv == ["happy", "go"]


def test_happy_pane_argv_refuses_a_pinned_session_id(monkeypatch) -> None:
    """happy strips --session-id and never re-adds it under its hook server, so a
    pinned uuid would make the receipt name a session that never exists."""
    import fno.agents.mux_spawn as mux_spawn
    from fno.agents.dispatch import DispatchAskError

    monkeypatch.setattr(mux_spawn.shutil, "which", lambda b: "/opt/homebrew/bin/happy")
    with pytest.raises(DispatchAskError, match="--session-id"):
        mux_spawn.happy_pane_argv(["claude", "--session-id", "u1", "go"], _ROUTE)


def test_happy_pane_argv_refuses_when_happy_is_absent(monkeypatch) -> None:
    import fno.agents.mux_spawn as mux_spawn
    from fno.agents.dispatch import DispatchAskError

    monkeypatch.setattr(mux_spawn.shutil, "which", lambda b: None)
    with pytest.raises(DispatchAskError, match="happy"):
        mux_spawn.happy_pane_argv(["claude", "go"], _ROUTE)


def test_explicit_happy_monitor_refuses_when_happy_is_absent_before_runner(
    tmp_path: Path, monkeypatch
) -> None:
    import fno.agents.mux_spawn as mux_spawn
    from fno.agents.dispatch import DispatchAskError
    from fno.agents.model_routing import resolve_explicit_route

    monkeypatch.setenv("ZAI_API_KEY", "zai-secret")
    monkeypatch.setattr(mux_spawn.shutil, "which", lambda binary: None)
    route = resolve_explicit_route("zai", "glm-5.2")
    assert route is not None
    runner = FakeRunner()

    with pytest.raises(DispatchAskError, match="--monitor happy") as exc:
        _spawn(
            monkeypatch,
            tmp_path,
            monitor="happy",
            route_provider="zai",
            route_env=route,
            runner=runner,
        )

    assert exc.value.exit_code == 127
    assert runner.calls == []


def test_explicit_happy_monitor_refuses_separate_model_before_runner(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.dispatch import DispatchAskError
    from fno.agents.model_routing import resolve_explicit_route

    monkeypatch.setenv("ZAI_API_KEY", "zai-secret")
    route = resolve_explicit_route("zai", "glm-5.2")
    assert route is not None
    runner = FakeRunner()
    with pytest.raises(DispatchAskError, match="separate --model") as exc:
        _spawn(
            monkeypatch,
            tmp_path,
            monitor="happy",
            route_provider="zai",
            route_env=route,
            model="claude-opus-4-1",
            runner=runner,
        )

    assert exc.value.exit_code == 2
    assert runner.calls == []


@pytest.mark.parametrize(
    ("provider", "route_provider", "message"),
    [("codex", "zai", "claude"), ("claude", "other", "zai")],
)
def test_explicit_happy_monitor_refuses_in_process_incompatible_route(
    tmp_path: Path,
    monkeypatch,
    provider: str,
    route_provider: str,
    message: str,
) -> None:
    from fno.agents.dispatch import DispatchAskError

    runner = FakeRunner()
    with pytest.raises(DispatchAskError, match=message) as exc:
        _spawn(
            monkeypatch,
            tmp_path,
            provider=provider,
            monitor="happy",
            route_provider=route_provider,
            route_env=dict(_ROUTE),
            runner=runner,
        )

    assert exc.value.exit_code == 2
    assert runner.calls == []


def test_explicit_happy_monitor_refuses_partial_zai_route_before_runner(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.dispatch import DispatchAskError

    runner = FakeRunner()
    with pytest.raises(DispatchAskError, match="resolved zai route") as exc:
        _spawn(
            monkeypatch,
            tmp_path,
            monitor="happy",
            route_provider="zai",
            route_env={"ANTHROPIC_MODEL": "glm-5.2"},
            runner=runner,
        )

    assert exc.value.exit_code == 2
    assert runner.calls == []


def test_explicit_happy_monitor_refuses_route_missing_canonical_tiers(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.dispatch import DispatchAskError

    monkeypatch.setenv("ZAI_API_KEY", "zai-secret")
    runner = FakeRunner()
    with pytest.raises(DispatchAskError, match="resolved zai route") as exc:
        _spawn(
            monkeypatch,
            tmp_path,
            monitor="happy",
            route_provider="zai",
            route_env={**_ROUTE, "ANTHROPIC_MODEL": "glm-5.2"},
            runner=runner,
        )

    assert exc.value.exit_code == 2
    assert runner.calls == []


def test_explicit_happy_monitor_refuses_route_that_does_not_match_zai(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.dispatch import DispatchAskError

    monkeypatch.setenv("ZAI_API_KEY", "zai-secret")
    runner = FakeRunner()
    with pytest.raises(DispatchAskError, match="resolved zai route") as exc:
        _spawn(
            monkeypatch,
            tmp_path,
            monitor="happy",
            route_provider="zai",
            route_env={
                "ANTHROPIC_BASE_URL": "https://api.anthropic.com",
                "ANTHROPIC_AUTH_TOKEN": "anthropic-secret",
                "ANTHROPIC_MODEL": "claude-opus-4-1",
            },
            runner=runner,
        )

    assert exc.value.exit_code == 2
    assert runner.calls == []


def test_explicit_happy_monitor_refuses_noncanonical_route_override(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.dispatch import DispatchAskError

    monkeypatch.setenv("ZAI_API_KEY", "zai-secret")
    runner = FakeRunner()
    with pytest.raises(DispatchAskError, match="resolved zai route") as exc:
        _spawn(
            monkeypatch,
            tmp_path,
            monitor="happy",
            route_provider="zai",
            route_env={
                **_ROUTE,
                "ANTHROPIC_MODEL": "glm-5.2",
                "ANTHROPIC_DEFAULT_OPUS_MODEL": "claude-opus-4-1",
            },
            runner=runner,
        )

    assert exc.value.exit_code == 2
    assert runner.calls == []


@pytest.mark.parametrize(
    "credential",
    ["ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN"],
)
def test_explicit_happy_monitor_refuses_conflicting_anthropic_credential(
    tmp_path: Path, monkeypatch, credential: str
) -> None:
    from fno.agents.dispatch import DispatchAskError

    monkeypatch.setenv("ZAI_API_KEY", "zai-secret")
    runner = FakeRunner()
    with pytest.raises(DispatchAskError, match="conflicting Anthropic credential") as exc:
        _spawn(
            monkeypatch,
            tmp_path,
            monitor="happy",
            route_provider="zai",
            route_env={
                **_ROUTE,
                "ANTHROPIC_MODEL": "glm-5.2",
                credential: "anthropic-secret",
            },
            runner=runner,
        )

    assert exc.value.exit_code == 2
    assert runner.calls == []


@pytest.mark.parametrize(
    "credential",
    ["ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN"],
)
def test_explicit_happy_monitor_refuses_conflicting_account_credential(
    tmp_path: Path, monkeypatch, credential: str
) -> None:
    from fno.agents.dispatch import DispatchAskError

    monkeypatch.setenv("ZAI_API_KEY", "zai-secret")
    runner = FakeRunner()
    with pytest.raises(DispatchAskError, match="conflicting Anthropic credential") as exc:
        _spawn(
            monkeypatch,
            tmp_path,
            monitor="happy",
            route_provider="zai",
            route_env={**_ROUTE, "ANTHROPIC_MODEL": "glm-5.2"},
            account_env={credential: "anthropic-secret"},
            runner=runner,
        )

    assert exc.value.exit_code == 2
    assert runner.calls == []


def _zai_route(monkeypatch):
    from fno.agents.model_routing import resolve_explicit_route

    monkeypatch.setenv("ZAI_API_KEY", "zai-secret")
    route = resolve_explicit_route("zai", "glm-5.2[1m]")
    assert route is not None
    return route


def _happy_spawn(monkeypatch, tmp_path, **kwargs):
    """A happy-monitored claude pane spawn.

    No real SessionStart hook fires under the fake runner, so the bounded
    registration wait is scripted: by default the worker "restamps" a known id
    (the success path); pass ``registration=(None, reason)`` to drive the
    failure path (reap + raise). The argv is built before that wait, so argv
    assertions are unaffected.
    """
    import fno.agents.mux_spawn as mux_spawn

    monkeypatch.setattr(mux_spawn.shutil, "which", lambda b: "/opt/homebrew/bin/happy")
    registration = kwargs.pop("registration", ("happy-proven-sid", ""))
    monkeypatch.setattr(
        mux_spawn, "_await_pane_registration", lambda name, mux, r, *a, **k: registration
    )
    return _spawn(
        monkeypatch,
        tmp_path,
        monitor="happy",
        route_provider="zai",
        route_env=_zai_route(monkeypatch),
        **kwargs,
    )


def test_happy_spawn_never_pins_a_session_id(tmp_path: Path, monkeypatch) -> None:
    """happy discards --session-id, so fno must not mint one and report it."""
    result, runner = _happy_spawn(monkeypatch, tmp_path)

    run_call = next(c for c in runner.calls if c[1:4] == ["mux", "pane", "run"])
    provider_argv = run_call[run_call.index("--") + 1 :]
    assert "--session-id" not in provider_argv
    assert "happy" in provider_argv
    # fno did not mint an id; the one on the receipt came from the restamp.
    assert result.session_uuid == "happy-proven-sid"


def test_happy_spawn_is_not_reported_live_without_a_proven_session(
    tmp_path: Path, monkeypatch
) -> None:
    """No proven session means fail loud, never strand a `spawning`/`live` corpse.

    The reported corpse was a real pid, a real pane, and a row reading
    `live`/`spawning` with no session behind it. Under the bounded-wait contract
    A happy pane that never registers is reaped and raised, not left as
    a silent no-op the receipt would call success.
    """
    from fno.agents.dispatch import DispatchAskError
    from fno.agents.registry import load_registry

    with pytest.raises(DispatchAskError, match="did not register"):
        _happy_spawn(
            monkeypatch, tmp_path, registration=(None, "no session id in window")
        )

    assert load_registry() == [], "the stranded row must be removed on failure"


def test_happy_spawn_never_guesses_from_the_transcript_store(
    tmp_path: Path, monkeypatch
) -> None:
    """A transcript appearing in the cwd during the spawn proves nothing.

    Two panes starting in one cwd see the same store; whichever writes its row
    first could claim the other's session. Binding the row to a healthy stranger
    is worse than leaving it unbound, so the spawn reads no transcripts at all.
    """
    import fno.agents.mux_spawn as mux_spawn
    from fno.agents.discover import PROJECTS_DIR_ENV, _candidate_dir_names

    projects = tmp_path / "projects"
    pdir = projects / _candidate_dir_names(str(tmp_path))[0]
    pdir.mkdir(parents=True)
    (pdir / "someone-elses-sid.jsonl").write_text("{}\n")
    monkeypatch.setenv(PROJECTS_DIR_ENV, str(projects))

    result, _ = _happy_spawn(monkeypatch, tmp_path, registration=("restamped-sid", ""))

    # The receipt uses the restamped id, never a guess read from the store.
    assert result.session_uuid == "restamped-sid"
    assert not hasattr(mux_spawn, "_backfill_claude_session_id")


def test_happy_routed_panes_config_read_failure_refuses(monkeypatch) -> None:
    import fno.agents.mux_spawn as mux_spawn
    import fno.config
    from fno.agents.dispatch import DispatchAskError

    def fail_to_load():
        raise ValueError("malformed config")

    monkeypatch.setattr(fno.config, "load_settings", fail_to_load)
    with pytest.raises(DispatchAskError, match="silently launching"):
        mux_spawn.happy_routed_panes_enabled()


def test_happy_routed_panes_malformed_config_file_refuses(
    tmp_path: Path, monkeypatch
) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.dispatch import DispatchAskError
    from fno.agents.mux_spawn import dispatch_spawn_pane
    from fno.config import load_settings

    config_path = tmp_path / "config.toml"
    config_path.write_text("[agents\nhappy_routed_panes = true\n", encoding="utf-8")
    monkeypatch.setenv("FNO_CONFIG", str(config_path))
    load_settings.cache_clear()
    runner = FakeRunner()

    try:
        with pytest.raises(DispatchAskError, match="silently launching"):
            dispatch_spawn_pane(
                name="peer",
                message="hello",
                provider="claude",
                cwd=tmp_path,
                route_env=dict(_ROUTE),
                runner=runner,
            )
        assert runner.calls == []
    finally:
        load_settings.cache_clear()


@pytest.mark.parametrize(
    "settings_args",
    [["--settings", "/tmp/r.json"], ["--settings=/tmp/r.json"]],
)
def test_happy_pane_argv_refuses_an_argv_carrying_settings(
    monkeypatch, settings_args: list[str]
) -> None:
    import fno.agents.mux_spawn as mux_spawn
    from fno.agents.dispatch import DispatchAskError

    monkeypatch.setattr(mux_spawn.shutil, "which", lambda b: "/opt/homebrew/bin/happy")
    with pytest.raises(DispatchAskError, match="--settings"):
        mux_spawn.happy_pane_argv(["claude", *settings_args, "go"], _ROUTE)


def _pane_run_argv(runner: "FakeRunner") -> list[str]:
    for call in runner.calls:
        if call[1:4] == ["mux", "pane", "run"]:
            return call[call.index("--") + 1 :]
    raise AssertionError("no `mux pane run` call recorded")


def test_codex_pane_applies_business_role_route_before_launch(tmp_path: Path, monkeypatch) -> None:
    from fno.agents import model_routing

    monkeypatch.setattr(
        model_routing,
        "resolve_codex_route",
        lambda role, **kwargs: model_routing.CodexRoute(
            env={"OPENAI_API_KEY": "business-key"},
            config_args=["-c", "model='gpt-business'"],
        ),
    )

    _, runner = _spawn(
        monkeypatch,
        tmp_path,
        provider="codex",
        role="publisher",
    )

    argv = _pane_run_argv(runner)
    assert "OPENAI_API_KEY=business-key" in argv
    codex_index = argv.index("codex")
    assert argv[codex_index : codex_index + 3] == [
        "codex",
        "-c",
        "model='gpt-business'",
    ]


def test_codex_pane_maps_business_role_error_before_launch(tmp_path: Path, monkeypatch) -> None:
    from fno.agents import model_routing
    from fno.agents.dispatch import DispatchAskError

    def blocked(*args, **kwargs):
        raise model_routing.BusinessRoleRoutingProjectionError("invalid pane business role")

    monkeypatch.setattr(model_routing, "resolve_codex_route", blocked)
    runner = FakeRunner()

    with pytest.raises(DispatchAskError, match="invalid pane business role") as exc_info:
        _spawn(
            monkeypatch,
            tmp_path,
            provider="codex",
            role="publisher",
            runner=runner,
        )

    assert exc_info.value.exit_code == 2
    assert runner.calls == []


def test_routed_claude_pane_launches_through_happy_when_enabled(
    tmp_path: Path, monkeypatch
) -> None:
    import fno.agents.mux_spawn as mux_spawn
    from fno.agents.model_routing import DEFAULT_ZAI_BASE_URL, resolve_explicit_route
    from fno.config import ConfigBlock, ModelRoutingBlock, SettingsModel

    monkeypatch.setattr(mux_spawn, "happy_routed_panes_enabled", lambda: True)
    monkeypatch.setattr(mux_spawn.shutil, "which", lambda b: "/opt/homebrew/bin/happy")
    monkeypatch.setattr(
        mux_spawn, "_await_pane_registration", lambda name, mux, r, *a, **k: ("happy-sid", "")
    )
    settings = SettingsModel(config=ConfigBlock(model_routing=ModelRoutingBlock()))
    route = resolve_explicit_route(
        "zai", "glm-5.2", settings=settings, env={"ZAI_API_KEY": "zai-secret"}
    )
    assert route is not None
    _, runner = _spawn(monkeypatch, tmp_path, route_env=route)

    argv = _pane_run_argv(runner)
    assert argv[0] == "env", "the mesh env wrapper must stay outermost"
    happy_argv = argv[argv.index("happy") :]
    pairs = [
        happy_argv[i + 1]
        for i, token in enumerate(happy_argv)
        if token == "--claude-env"
    ]
    assert f"ANTHROPIC_BASE_URL={DEFAULT_ZAI_BASE_URL}" in pairs
    # Credential on the env(1) run only -- see the union invariant test.
    assert not any(p.startswith("ANTHROPIC_AUTH_TOKEN=") for p in pairs)
    assert "ANTHROPIC_AUTH_TOKEN=zai-secret" in argv[: argv.index("happy")]
    assert "--settings" not in argv


def test_unrouted_and_disabled_panes_still_launch_plain_claude(
    tmp_path: Path, monkeypatch
) -> None:
    import fno.agents.mux_spawn as mux_spawn

    monkeypatch.setattr(mux_spawn, "happy_routed_panes_enabled", lambda: True)
    _, runner = _spawn(monkeypatch, tmp_path)
    assert "happy" not in _pane_run_argv(runner), "an unrouted pane must not use happy"

    monkeypatch.setattr(mux_spawn, "happy_routed_panes_enabled", lambda: False)
    _, runner2 = _spawn(monkeypatch, tmp_path, name="peer2", route_env=dict(_ROUTE))
    assert "happy" not in _pane_run_argv(runner2), "knob off must leave the argv alone"


# A happy-hosted claude pane is created id-less (`spawning`) because
# happy owns the session id and restamps the row later via the worker's
# SessionStart hook. If that restamp never lands, the row strands `spawning`
# forever and the receipt reads as a soft success - a silent no-op. The fix
# waits for the restamp within a bounded window, then either hands back a
# `live` receipt or reaps the pane and fails loud.


def _reg_row(name: str = "peer", hsid: Optional[str] = None):
    from fno.agents.registry import AgentEntry

    return AgentEntry(
        name=name,
        harness="claude",
        cwd="/w",
        log_path="",
        status="spawning",
        harness_session_id=hsid,
        mux={"session": "main", "pane_id": 7},
    )


def test_await_pane_registration_returns_id_once_restamped(monkeypatch) -> None:
    import fno.agents.mux_spawn as mux_spawn

    calls = {"n": 0}

    def fake_load(*a, **k):
        calls["n"] += 1
        return [_reg_row(hsid="sess-1" if calls["n"] > 1 else None)]

    monkeypatch.setattr(mux_spawn, "load_registry", fake_load)
    monkeypatch.setattr(mux_spawn, "_PANE_REGISTRATION_POLL_S", 0.001)
    sid, reason = mux_spawn._await_pane_registration(
        "peer", {"session": "main", "pane_id": 7}, FakeRunner(wait_returncode=11)
    )
    assert sid == "sess-1"
    assert reason == ""


def test_await_pane_registration_fast_fails_a_dead_pane(monkeypatch) -> None:
    import fno.agents.mux_spawn as mux_spawn

    monkeypatch.setattr(mux_spawn, "load_registry", lambda *a, **k: [_reg_row(hsid=None)])
    # wait returncode 12 == EXIT_WAIT_EXITED: the pane child is gone, so no
    # restamp is ever coming. Must short-circuit, not sit out the full window.
    sid, reason = mux_spawn._await_pane_registration(
        "peer", {"session": "main", "pane_id": 7}, FakeRunner(wait_returncode=12)
    )
    assert sid is None
    assert "exited" in reason


def test_await_pane_registration_times_out_when_alive_but_unregistered(monkeypatch) -> None:
    import fno.agents.mux_spawn as mux_spawn

    monkeypatch.setattr(mux_spawn, "load_registry", lambda *a, **k: [_reg_row(hsid=None)])
    monkeypatch.setattr(mux_spawn, "_PANE_REGISTRATION_DEADLINE_S", 0.01)
    monkeypatch.setattr(mux_spawn, "_PANE_REGISTRATION_POLL_S", 0.001)
    # Alive (11) but never registered: the reproduced failure shape.
    sid, reason = mux_spawn._await_pane_registration(
        "peer", {"session": "main", "pane_id": 7}, FakeRunner(wait_returncode=11)
    )
    assert sid is None
    assert "no session id" in reason


def test_happy_pane_failure_reaps_and_raises_not_silent(
    tmp_path: Path, monkeypatch
) -> None:
    """The headline: a stranded happy pane must fail loud, not return `spawning`."""
    import fno.agents.mux_spawn as mux_spawn
    from fno.agents.dispatch import DispatchAskError
    from fno.agents.registry import load_registry

    monkeypatch.setattr(mux_spawn, "happy_routed_panes_enabled", lambda: True)
    monkeypatch.setattr(mux_spawn.shutil, "which", lambda b: "/usr/local/bin/happy")
    monkeypatch.setattr(
        mux_spawn,
        "_await_pane_registration",
        lambda name, mux, r, *a, **k: (None, "pane process exited before registering"),
    )
    runner = FakeRunner()
    with pytest.raises(DispatchAskError) as ei:
        _spawn(monkeypatch, tmp_path, route_env=dict(_ROUTE), runner=runner)

    assert ei.value.exit_code == 1
    assert "did not register" in str(ei.value)
    assert runner.kill_calls, "the stranded pane must be reaped"
    assert load_registry() == [], "the stranded spawning row must be removed"


def test_happy_pane_failure_reports_row_removal_failure_honestly(
    tmp_path: Path, monkeypatch
) -> None:
    """If the row removal throws after a successful reap, the error must not
    claim 'row removed'. The row lingers and the message
    names it instead of lying about cleanup."""
    import fno.agents.mux_spawn as mux_spawn
    from fno.agents.dispatch import DispatchAskError
    from fno.agents.registry import load_registry

    monkeypatch.setattr(mux_spawn, "happy_routed_panes_enabled", lambda: True)
    monkeypatch.setattr(mux_spawn.shutil, "which", lambda b: "/usr/local/bin/happy")
    monkeypatch.setattr(
        mux_spawn,
        "_await_pane_registration",
        lambda name, mux, r, *a, **k: (None, "no session id in window"),
    )
    # Let the create-write (_append) succeed, then fail the removal write.
    real_update = mux_spawn.update_registry
    state = {"n": 0}

    def flaky(*a, **k):
        state["n"] += 1
        if state["n"] >= 2:
            raise OSError("registry locked")
        return real_update(*a, **k)

    monkeypatch.setattr(mux_spawn, "update_registry", flaky)
    runner = FakeRunner()  # kill_returncode=0 -> reaped=True
    with pytest.raises(DispatchAskError) as ei:
        _spawn(monkeypatch, tmp_path, route_env=dict(_ROUTE), runner=runner)

    msg = str(ei.value)
    assert "row removal failed" in msg, "must not claim 'row removed' when it was not"
    assert load_registry() != [], "the row must still be present (removal failed)"


def test_happy_pane_success_returns_live_receipt(
    tmp_path: Path, monkeypatch
) -> None:
    """When the restamp lands, the receipt earns `live` + a real short_id."""
    import fno.agents.mux_spawn as mux_spawn

    monkeypatch.setattr(mux_spawn, "happy_routed_panes_enabled", lambda: True)
    monkeypatch.setattr(mux_spawn.shutil, "which", lambda b: "/usr/local/bin/happy")
    monkeypatch.setattr(
        mux_spawn,
        "_await_pane_registration",
        lambda name, mux, r, *a, **k: ("sess-live-1", ""),
    )
    result, _ = _spawn(monkeypatch, tmp_path, route_env=dict(_ROUTE))
    assert result.status == "live"
    assert result.short_id, "a live receipt must carry a non-empty short_id"


def test_claude_pane_argv_carries_the_worker_name(tmp_path: Path) -> None:
    """The pane lane forwards `--name`, like the bg lane always has.

    Without it claude names the session from the launching session's lineage,
    so every pane worker on one box shows the SAME display name and a session
    list cannot tell N workers apart. Asserting the flag is PRESENT is not
    enough: it has to carry THIS worker's name, since a lineage-inherited
    string is also a non-empty name and would pass a presence check.
    """
    from fno.agents.mux_spawn import build_pane_argv

    argv = build_pane_argv(
        "claude", "task", tmp_path, False, "uuid-1", name="target-x-e4bf-ca-enf"
    )
    assert argv == [
        "claude",
        "--session-id",
        "uuid-1",
        "--name",
        "target-x-e4bf-ca-enf",
        "task",
    ]

    # No name resolved -> today's argv, byte for byte. An empty `--name` would
    # be worse than none: claude would take the empty string as the display name.
    assert build_pane_argv("claude", "task", tmp_path, False, "uuid-1", name="") == [
        "claude",
        "--session-id",
        "uuid-1",
        "task",
    ]

    # Only claude is wired. The other arms have no verified equivalent flag, so
    # a name must not leak into their argv as a stray token.
    for provider in ("codex", "gemini", "agy", "opencode"):
        assert "--name" not in build_pane_argv(
            provider, "task", tmp_path, False, None, name="target-x-e4bf-ca-enf"
        ), f"{provider} pane argv must not grow an unverified --name"


def test_a_terminal_row_does_not_own_its_name(tmp_path: Path, monkeypatch) -> None:
    """x-cdca: a dead pane must not deadlock the node that spawned it.

    `fno dispatch one` releases its claim and lane on a failed spawn and retries
    under the SAME deterministic worker name. A status-blind collision guard
    therefore turned one dead pane into a permanently failed node until a human
    ran `fno agents rm`. A terminal row will never act again, so it does not
    hold the name; its evidence lives in a timestamped file on disk, not in the
    registry row.
    """
    from dataclasses import replace as _replace

    from fno.agents.registry import load_registry, update_registry

    first, _ = _spawn(monkeypatch, tmp_path, name="w1")
    assert first.name == "w1"

    # Make the existing row terminal, the way the death branch does.
    update_registry(lambda rows: [_replace(r, status="failed") for r in rows])

    second, _ = _spawn(monkeypatch, tmp_path, name="w1")
    assert second.name == "w1"
    rows = [r for r in load_registry() if r.name == "w1"]
    assert len(rows) == 1, "the corpse must be dropped in the same transaction"
    assert rows[0].status != "failed"


def test_a_live_row_still_owns_its_name(tmp_path: Path, monkeypatch) -> None:
    """The reclaim is scoped to terminal rows: a live worker is still protected."""
    from fno.agents.dispatch import DispatchAskError

    _spawn(monkeypatch, tmp_path, name="w2")
    with pytest.raises(DispatchAskError) as exc:
        _spawn(monkeypatch, tmp_path, name="w2")
    assert exc.value.exit_code == 2
