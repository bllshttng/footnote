"""Unit tests for the join-partition PreToolUse write guard.

The guard is driven exactly as claude drives it: a JSON PreToolUse payload on
stdin, the worker's environment in the process env, one JSON decision on
stdout. The fast path (no ``.fno/join-partition/`` in the payload cwd) must
approve without spawning any Python - it fires on every Edit and Write in
every session. Everything past it is judged per target, and ONE denied target
denies the whole call: safe siblings cannot launder an unsafe one.
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

HOOK = Path(__file__).resolve().parents[3] / "hooks" / "join-partition-write-guard.sh"


def _policy_dir(worktree: Path) -> Path:
    d = worktree / ".fno" / "join-partition"
    d.mkdir(parents=True, exist_ok=True)
    return d


def _write_policy(
    worktree: Path, name: str, band: str, deny_edit: list[str], own: list[str]
) -> None:
    policy = {
        "band": band,
        "verdict": "enforced",
        "allow_write": [*own, ".git/", ".fno/"],
        "deny_edit": deny_edit,
        "sandbox": {"enabled": True, "filesystem": {"denyWrite": []}},
    }
    _policy_dir(worktree).joinpath(f"{name}.json").write_text(json.dumps(policy))


def _run(payload: dict, *, worker: str = "", cwd: str | None = None):
    env = {"PATH": "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin", "HOME": str(Path.home())}
    if worker:
        env["FNO_WORKER_NAME"] = worker
    proc = subprocess.run(
        ["bash", str(HOOK)],
        input=json.dumps(payload),
        capture_output=True,
        text=True,
        env=env,
        cwd=cwd or str(Path(__file__).parent),
        timeout=120,
    )
    return json.loads(proc.stdout)


def _payload(worktree: Path, file_path: str, **extra) -> dict:
    payload = {"cwd": str(worktree), "tool_name": "Edit", "tool_input": {"file_path": file_path}}
    payload.update(extra)
    return payload


def test_absent_policy_dir_approves(tmp_path):
    """AC6-EDGE fast path: not a joined worktree means approve, zero Python."""
    decision = _run(_payload(tmp_path, "src/anything.py"))
    assert decision == {}


def test_joiner_denied_on_a_peer_band_file(tmp_path):
    """AC3-HP: an Edit in deny_edit is denied, reason naming the owning band."""
    _write_policy(tmp_path, "j-x-1", "high", ["cli/src/fno/peer.py"], own=["src/own.py"])
    _write_policy(
        tmp_path, "j-x-2", "medium", ["src/own.py"], own=["cli/src/fno/peer.py"]
    )
    decision = _run(
        _payload(tmp_path, str(tmp_path / "cli/src/fno/peer.py")),
        worker="j-x-1",
    )
    assert decision["decision"] == "block"
    assert "medium" in decision["reason"]


def test_joiner_approved_on_own_band_file(tmp_path):
    _write_policy(tmp_path, "j-x-1", "high", ["cli/src/fno/peer.py"], own=["src/own.py"])
    decision = _run(_payload(tmp_path, str(tmp_path / "src/own.py")), worker="j-x-1")
    assert decision == {}


def test_apply_patch_one_denied_target_denies_all(tmp_path):
    _write_policy(tmp_path, "j-x-1", "high", ["cli/src/fno/peer.py"], own=["src/own.py"])
    command = (
        "*** Begin Patch\n*** Update File: src/own.py\n@@\n"
        "*** Update File: cli/src/fno/peer.py\n@@\n*** End Patch"
    )
    decision = _run(
        _payload(tmp_path, "unused", tool_input={"command": command}),
        worker="j-x-1",
    )
    assert decision["decision"] == "block"


def test_worker_without_a_policy_file_is_not_jailed(tmp_path):
    """LD3: the holder (or any unjailed session) keeps writing."""
    _write_policy(tmp_path, "j-x-1", "high", ["cli/src/fno/peer.py"], own=["src/own.py"])
    decision = _run(_payload(tmp_path, str(tmp_path / "cli/src/fno/peer.py")), worker="holder-x")
    assert decision == {}


def test_unresolvable_identity_is_not_jailed(tmp_path):
    """No FNO_WORKER_NAME and no roster row: the guard cannot attribute, so it
    cannot jail - the OS layer still covers a real joiner, which resolves."""
    _write_policy(tmp_path, "j-x-1", "high", ["cli/src/fno/peer.py"], own=["src/own.py"])
    decision = _run(_payload(tmp_path, str(tmp_path / "cli/src/fno/peer.py")))
    assert decision == {}
