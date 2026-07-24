"""Regression tests for scripts/memory/append-lesson-candidate.sh.

The dual-emit helper stages a load-bearing project lesson to
~/.fno/lesson-candidates.jsonl. It must append one valid JSON line on success
and ALWAYS exit 0 (warn on failure) so it never blocks the memory write or merge.
"""
import json
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
APPEND = ROOT / "scripts" / "memory" / "append-lesson-candidate.sh"

CANDIDATE = (
    '{"type":"project","name":"guard-on-one-path",'
    '"description":"guards on one of N paths are decorative",'
    '"body":"enumerate every reachable path before trusting a guard"}'
)


def _run(*args: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["bash", str(APPEND), *args],
        capture_output=True,
        text=True,
    )


def test_valid_candidate_appends_one_line(tmp_path: Path) -> None:
    out = tmp_path / "lesson.jsonl"
    r = _run("--candidate", CANDIDATE, "--session-id", "sess-1", "--file", str(out))
    assert r.returncode == 0, r.stderr
    lines = out.read_text(encoding="utf-8").splitlines()
    assert len(lines) == 1
    rec = json.loads(lines[0])
    assert rec["source_session"] == "sess-1"
    assert rec["name"] == "guard-on-one-path"
    assert rec["type"] == "project"


def test_failed_append_warns_and_never_blocks(tmp_path: Path) -> None:
    # Parent dir is a regular file, so mkdir -p and the append both fail.
    blocker = tmp_path / "blocker"
    blocker.write_text("")
    r = _run("--candidate", CANDIDATE, "--file", str(blocker / "lesson.jsonl"))
    assert r.returncode == 0, "a failed append must exit 0, never block"
    assert "not staged (non-fatal)" in r.stderr


def test_malformed_candidate_rejected_and_unblocks(tmp_path: Path) -> None:
    out = tmp_path / "lesson.jsonl"
    out.write_text("")  # ensure exists; must stay empty
    r = _run("--candidate", "not-json", "--file", str(out))
    assert r.returncode == 0
    assert "jq rejected" in r.stderr
    assert out.read_text(encoding="utf-8") == "", "malformed candidate must not write"


def test_missing_value_exits_zero() -> None:
    # A flag with no value must warn + exit 0, never abort the caller (AC7-FR).
    r = _run("--candidate")
    assert r.returncode == 0
    assert "missing value" in r.stderr
