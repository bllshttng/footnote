"""Slow, real-binary Codex pane journeys."""
from __future__ import annotations

import io
import json
import os
import shutil
import subprocess
import time
import uuid
from pathlib import Path

import pytest

from fno.paths_testing import use_tmpdir

CODEX_HARNESS = "codex"

def _real_mux_binaries(repo: Path) -> tuple[Path, Path] | None:
    """Return binaries built by the CI cargo step when both are executable."""
    fno_bin = repo / "crates" / "fno" / "target" / "debug" / "fno"
    worker_bin = (
        repo / "crates" / "fno-agents" / "target" / "debug" / "fno-agents-worker"
    )
    missing = [
        str(path.relative_to(repo))
        for path in (fno_bin, worker_bin)
        if not path.is_file() or not os.access(path, os.X_OK)
    ]
    if missing:
        return None
    return fno_bin, worker_bin


@pytest.mark.slow_e2e
@pytest.mark.timeout(144)
def test_late_codex_identity_composes_across_every_peer_surface(
    tmp_path: Path, monkeypatch
) -> None:
    """Late harness binding preserves the pane's stable Footnote identity."""
    use_tmpdir(monkeypatch, tmp_path)
    repo = Path(__file__).resolve().parents[3]
    binaries = _real_mux_binaries(repo)
    if binaries is None:
        pytest.skip("prebuilt fno and fno-agents-worker binaries are required")
    fno_bin, worker_bin = binaries

    agents_home = tmp_path / ".fno" / "agents"
    mux_dir = Path("/tmp") / f"fno-i-{os.getpid()}-{uuid.uuid4().hex[:6]}"
    mux_dir.mkdir()
    monkeypatch.setenv("FNO_BIN", str(fno_bin))
    monkeypatch.setenv("FNO_AGENTS_WORKER_BIN", str(worker_bin))
    monkeypatch.setenv("FNO_AGENTS_HOME", str(agents_home))
    monkeypatch.setenv("FNO_MUX_DIR", str(mux_dir))
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claim-root"))
    monkeypatch.setenv("FNO_E2E", "1")
    monkeypatch.setenv("FNO_PROCESS_ADMISSION_MAX", "512")
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
            'exec 3<"$1"; (sleep 3; printf "seed accepted\\n") & read submitted; printf "%s\\n" "$submitted"; sleep 20',
            "sh",
            str(rollout),
        ],
    )
    # The daemon start would exec a real provider binary; the journey
    # exercises the late-identity heal, not the daemon contract.
    from fno.agents import codex_pane

    monkeypatch.setattr(codex_pane, "ensure_codex_daemon", lambda *_a, **_k: None)

    spawned = None
    try:
        spawned = mux_spawn.dispatch_spawn_pane(
            name=requested_name,
            message="wait",
            provider=CODEX_HARNESS,
            cwd=repo,
            session=mux_session,
        )
        assert spawned.status == "live"
        assert spawned.session_uuid is not None
        assert spawned.short_id == spawned.session_uuid[:8]

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
        birth_row = load_registry(path=agents_home / "registry.json")[0]
        footnote_identity = birth_row.fno_id
        assert footnote_identity is not None
        probe_pid = birth_row.pid
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
        assert reconciled.backfilled == []
        identity = spawned.session_uuid

        registry_path = agents_home / "registry.json"
        row = load_registry(path=registry_path)[0]
        assert row.harness_session_id == identity
        assert row.fno_id == footnote_identity
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
            [str(fno_bin), "mux", "pane", "ls", "--server", mux_session, "--json"],
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
        assert pane["fno_id"] == footnote_identity
        assert pane["harness_session_id"] == identity
        assert pane["fno_id"] == row.fno_id
        assert row.fno_id != identity
        located = subprocess.run(
            [str(fno_bin), "mux", "where", identity, "--server", mux_session, "--json"],
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
        assert {
            row.harness_session_id,
            pane["harness_session_id"],
            claim.holder,
            *resolved,
        } == {identity}
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


@pytest.mark.slow_e2e
@pytest.mark.timeout(144)
def test_codex_autonomous_pane_journey_completes_without_operator_input(
    tmp_path: Path, monkeypatch
) -> None:
    """A fake Codex pane receives its task, exits, and leaves readable output."""
    use_tmpdir(monkeypatch, tmp_path)
    repo = Path(__file__).resolve().parents[3]
    binaries = _real_mux_binaries(repo)
    if binaries is None:
        pytest.skip("prebuilt fno and fno-agents-worker binaries are required")
    fno_bin, worker_bin = binaries

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
        "FNO_AGENTS_WORKER_BIN": str(worker_bin),
        "FNO_AGENTS_HOME": str(agents_home),
        "FNO_MUX_DIR": str(mux_dir),
        "FNO_CLAIMS_ROOT": str(tmp_path / "claims"),
        "FNO_E2E": "1",
        "FNO_PROCESS_ADMISSION_MAX": "512",
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
            "sleep 30",
        ],
        cwd=repo,
        env=env,
        text=True,
        capture_output=True,
    )
    assert keeper.returncode == 0, keeper.stderr

    import fno.agents.mux_spawn as mux_spawn
    from fno.agents.mux_spawn import dispatch_spawn_pane

    monkeypatch.setattr(
        mux_spawn,
        "_backfill_codex_session_id",
        lambda *_args, **_kwargs: "019fb024-2327-75f3-8b80-06e9d5ade05f",
    )

    spawned = None
    try:
        spawned = dispatch_spawn_pane(
            name="autonomous-codex-proof",
            message="AUTONOMOUS-PANE-TASK",
            provider="codex",
            cwd=repo,
            session=session,
            yolo=True,
            codex_sessions_dir=tmp_path / "no-rollouts",
        )

        deadline = time.monotonic() + 10.0
        while time.monotonic() < deadline and not prompt_file.exists():
            time.sleep(0.05)
        prompt = prompt_file.read_text(encoding="utf-8")
        assert prompt.startswith("AUTONOMOUS-PANE-TASK\n\n")
        assert prompt.count("<fno_relay_compression>") == 1

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
