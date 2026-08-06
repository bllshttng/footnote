"""emit-attestation.sh records the attesting actor on review_attestation.

The producer resolves session_id + harness from the live session manifest so a
review_attestation carries WHO certified the diff alongside WHAT. Without it an
author attesting its own diff is indistinguishable from an independent reviewer.
This is the producer-level check; the validator-level accept/reject lives in
test_python_validator.py and the parity corpus.
"""
from __future__ import annotations

import json
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
    r = subprocess.run(
        ["bash", str(_SCRIPT), "sigma", "pass"],
        cwd=repo, capture_output=True, text=True,
    )
    assert r.returncode == 0, r.stderr
    ev = _last_event(repo)
    assert ev["type"] == "review_attestation"
    assert ev["data"]["session_id"] == "20260806T225503Z-cl84104-d4f619"
    assert ev["data"]["harness"] == "claude"
