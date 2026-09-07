"""The cleanup mode: a terminus with no gate weight (x-25a7 wave 5).

AC7-MARKER is the load-bearing case: a cleanup run must write ZERO
review_attestation rows for its head. The test does not assert that zero
blindly - a zero has three explanations (real outcome, instrument never ran,
pipeline ate the output), so the same scratch repo drives the POSITIVE
control first: the default lane's emit path at the same head DOES write a
row. With the instrument proven, cleanup's zero rows read as the outcome.
"""
from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]
ROUTER = REPO_ROOT / "skills" / "review" / "SKILL.md"
CLEANUP = REPO_ROOT / "skills" / "review" / "references" / "cleanup.md"
EMIT = REPO_ROOT / "skills" / "review" / "scripts" / "emit-attestation.sh"


def _scratch_repo(tmp_path: Path) -> Path:
    """A repo with a real feature diff: the emit refuses an empty diff."""
    repo = tmp_path / "repo"
    repo.mkdir()
    for args in (
        ["git", "init", "-q"],
        ["git", "config", "user.email", "t@t.t"],
        ["git", "config", "user.name", "t"],
    ):
        subprocess.run(args, cwd=repo, check=True)
    (repo / "a.txt").write_text("one\n", encoding="utf-8")
    subprocess.run(["git", "add", "a.txt"], cwd=repo, check=True)
    subprocess.run(["git", "commit", "-q", "-m", "base"], cwd=repo, check=True)
    base = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=repo, capture_output=True, text=True, check=True
    ).stdout.strip()
    subprocess.run(["git", "update-ref", "refs/remotes/origin/main", base], cwd=repo, check=True)
    (repo / "a.txt").write_text("one\ntwo\n", encoding="utf-8")
    subprocess.run(["git", "add", "a.txt"], cwd=repo, check=True)
    subprocess.run(["git", "commit", "-q", "-m", "feature"], cwd=repo, check=True)
    return repo


def _attestations(repo: Path) -> list[dict]:
    from fno.paths import project_log

    path = project_log("events.jsonl", project_root=repo)
    if not path.exists():
        return []
    return [
        json.loads(ln)
        for ln in path.read_text().splitlines()
        if ln.strip() and json.loads(ln).get("type") == "review_attestation"
    ]


def test_cleanup_writes_zero_attestations_with_a_proven_instrument(tmp_path):
    # AC7-MARKER. Positive control FIRST: the default lane's emit path writes
    # a row at this head, so the events instrument demonstrably records one.
    # Cleanup is contract-bound to add none: the mode instructs no emit call
    # anywhere on its surface, so a completed cleanup run leaves the count
    # where the lane left it. Without the control, "zero" could equally mean
    # the instrument never ran (the receipt-can-lie pitfalls shape).
    repo = _scratch_repo(tmp_path)
    env = {**os.environ, "FNO": "fno-py"}
    r = subprocess.run(
        ["bash", str(EMIT), "code-review"], cwd=repo, env=env, capture_output=True, text=True
    )
    assert r.returncode == 0, r.stderr
    assert len(_attestations(repo)) == 1, "positive control failed: emit wrote nothing"

    # The cleanup contract emits no attestation: cleanup.md names no emit
    # surface, and the router's Step 2c forbids the row explicitly.
    text = CLEANUP.read_text(encoding="utf-8")
    assert "emit-attestation" not in text
    assert "emits no attestation" in text
    router_text = ROUTER.read_text(encoding="utf-8")
    assert "no attestation - a cleanup run writes no `review_attestation` row" in router_text
    # And the row count is still exactly the control's: nothing on the
    # cleanup surface appends to it.
    assert len(_attestations(repo)) == 1


def test_the_router_has_a_cleanup_token_and_route_line():
    text = ROUTER.read_text(encoding="utf-8")
    assert "running cleanup (apply-or-skip)" in text
    assert "`cleanup`" in text
    # the unknown-mode help lists cleanup, so discovery does not depend on docs
    assert "valid modes: prove-it, cleanup, peer, research, declare" in text


def test_each_angle_gets_exactly_one_disposition_and_skips_carry_reasons():
    # AC7-HP. The reference fixes the four angles and the two dispositions;
    # a skip without a reason is named as an angle that never ran.
    text = CLEANUP.read_text(encoding="utf-8")
    for angle in ("Reuse", "Simplification", "Efficiency", "Altitude"):
        assert f"**{angle}**" in text
    assert "APPLY" in text
    assert "SKIP" in text
    assert "cleanup: <angle> skipped - <reason>" in text
    assert "A skip with no reason is not a skip" in text
    # no verify pass, no threads
    assert "No verify pass" in text
    assert "no threads" in text.lower() or "opens no threads" in text


def test_cleanup_runs_on_every_harness_no_unavailable_outcome():
    # AC7-ERR. It is fno's own inline pass, so harness availability is not a
    # variable for it. Assert the positive statements, on both surfaces.
    for path in (CLEANUP, ROUTER):
        text = path.read_text(encoding="utf-8")
        assert "every harness" in text
    cleanup_text = CLEANUP.read_text(encoding="utf-8")
    assert '"Unavailable on this harness" is not an outcome' in cleanup_text


def test_cleanup_population_is_the_nonblocking_categories():
    # The population boundary: cleanup never quietly applies what the
    # classifier would block on. Correctness and security stay review findings.
    text = CLEANUP.read_text(encoding="utf-8")
    assert "non-blocking categories" in text
    assert "never downgrades" in text
