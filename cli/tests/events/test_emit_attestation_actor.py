"""emit-attestation.sh records the attesting actor on review_attestation.

The producer resolves session_id + harness from the live session manifest so a
review_attestation carries WHO certified the diff alongside WHAT. Without it an
author attesting its own diff is indistinguishable from an independent reviewer.
This is the producer-level check; the validator-level accept/reject lives in
test_python_validator.py and the parity corpus.
"""
from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path

_REPO_ROOT = Path(__file__).resolve().parents[3]
_SCRIPT = _REPO_ROOT / "skills" / "review" / "scripts" / "emit-attestation.sh"


def _temp_git_repo(tmp_path: Path, manifest: str | None) -> Path:
    """A throwaway git repo with one commit; optionally a session manifest."""
    sub = tmp_path / "repo"
    sub.mkdir()
    for args in (
        ["git", "init", "-q"],
        ["git", "config", "user.email", "t@t.t"],
        ["git", "config", "user.name", "t"],
        ["git", "commit", "-q", "--allow-empty", "-m", "init"],
    ):
        subprocess.run(args, cwd=sub, check=True)
    if manifest is not None:
        (sub / ".fno").mkdir()
        (sub / ".fno" / "target-state.md").write_text(manifest)
    return sub


def _last_event(repo: Path) -> dict:
    lines = [
        ln for ln in (repo / ".fno" / "events.jsonl").read_text().splitlines()
        if ln.strip()
    ]
    return json.loads(lines[-1])


def test_attestation_records_session_and_harness(tmp_path: Path) -> None:
    """A manifest present -> the emitted attestation carries the actor."""
    manifest = (
        "session_id: 20260806T225503Z-cl84104-d4f619\n"
        "harness: claude\n"
    )
    repo = _temp_git_repo(tmp_path, manifest)
    env = {**os.environ, "FNO": "fno-py"}
    r = subprocess.run(
        ["bash", str(_SCRIPT), "sigma", "pass"],
        cwd=repo, env=env, capture_output=True, text=True,
    )
    assert r.returncode == 0, r.stderr
    ev = _last_event(repo)
    assert ev["type"] == "review_attestation"
    assert ev["data"]["session_id"] == "20260806T225503Z-cl84104-d4f619"
    assert ev["data"]["harness"] == "claude"


def test_attestation_records_the_routed_model(tmp_path: Path) -> None:
    """A routed session -> the attestation names the model that rendered the
    verdict. Routing stamps ANTHROPIC_MODEL for the whole worker process, so a
    build-lane worker reviews its own diff on the routed model and no per-spawn
    role guard can see it."""
    repo = _temp_git_repo(tmp_path, "session_id: s\nharness: claude\n")
    env = {
        **os.environ,
        "FNO": "fno-py",
        "ANTHROPIC_MODEL": "glm-5.2[1m]",
        # Userinfo included on purpose: a base_url may carry a credential, and
        # the event log is durable - neither the path nor the key may land in it.
        "ANTHROPIC_BASE_URL": "https://sk-secret@api.z.ai/api/anthropic",
    }
    r = subprocess.run(
        ["bash", str(_SCRIPT), "code-review", "pass"],
        cwd=repo, env=env, capture_output=True, text=True,
    )
    assert r.returncode == 0, r.stderr
    data = _last_event(repo)["data"]
    assert data["model"] == "glm-5.2[1m]"
    assert data["provider"] == "api.z.ai"
    assert "sk-secret" not in json.dumps(data) and "sk-secret" not in r.stderr


def test_attestation_model_is_empty_not_guessed_when_unrouted(tmp_path: Path) -> None:
    """No routing -> empty, never a guessed default. A fabricated model name
    would be read later as evidence of which model reviewed."""
    repo = _temp_git_repo(tmp_path, "session_id: s\nharness: claude\n")
    env = {
        k: v
        for k, v in os.environ.items()
        if k not in ("ANTHROPIC_MODEL", "ANTHROPIC_BASE_URL")
    }
    env["FNO"] = "fno-py"
    r = subprocess.run(
        ["bash", str(_SCRIPT), "code-review", "pass"],
        cwd=repo, env=env, capture_output=True, text=True,
    )
    assert r.returncode == 0, r.stderr
    data = _last_event(repo)["data"]
    assert data["model"] == ""
    assert data["provider"] == ""
