"""Tests for the pre-PR stale-base guard (_preflight.check_stale_base).

Exercises the exit-code contract (0 fresh / 3 stale / 4 unrelated) against REAL
temp git repos with back-dated commits, plus the bypass + gate_escape emit and
the fail-open paths. Committer date is the staleness metric, so commits are
dated via GIT_{AUTHOR,COMMITTER}_DATE.
"""
from __future__ import annotations

import json
import subprocess
from datetime import datetime, timedelta, timezone
from pathlib import Path

from fno.pr import _preflight


def _git(cwd: Path, *args: str, env: dict | None = None) -> str:
    full = {**__import__("os").environ, **(env or {})}
    return subprocess.run(
        ["git", *args], cwd=cwd, text=True, capture_output=True, check=True, env=full
    ).stdout


def _date_env(dt: datetime) -> dict:
    iso = dt.isoformat()
    return {"GIT_AUTHOR_DATE": iso, "GIT_COMMITTER_DATE": iso}


def _commit(work: Path, fname: str, when: datetime, msg: str) -> None:
    (work / fname).write_text(f"{fname}\n")
    _git(work, "add", "-A")
    _git(work, "commit", "-m", msg, env=_date_env(when))


def _repo(tmp_path: Path, mb_age: timedelta) -> Path:
    """Work repo + bare origin. origin/main tip is NOW; the merge-base of
    ``feature/x`` with origin/main is ``mb_age`` old, so span == mb_age."""
    now = datetime.now(timezone.utc)
    bare = tmp_path / "origin.git"
    subprocess.run(["git", "init", "--bare", "-b", "main", str(bare)], check=True)
    work = tmp_path / "work"
    subprocess.run(["git", "init", "-b", "main", str(work)], check=True)
    _git(work, "config", "user.email", "t@t.t")
    _git(work, "config", "user.name", "t")
    _commit(work, "base.txt", now - mb_age, "init")  # becomes the merge-base
    _git(work, "remote", "add", "origin", str(bare))
    _git(work, "push", "-u", "origin", "main")
    # Feature branch off the (old) merge-base.
    _git(work, "checkout", "-b", "feature/x")
    _commit(work, "feature.txt", now, "feature work")
    # Advance origin/main to NOW so the tip is fresh but the merge-base is old.
    _git(work, "checkout", "main")
    _commit(work, "main-advance.txt", now, "main advances")
    _git(work, "push", "origin", "main")
    _git(work, "checkout", "feature/x")
    return work


def test_fresh_branch_passes(tmp_path):  # AC1-HP
    work = _repo(tmp_path, timedelta(hours=2))
    code, msg = _preflight.check_stale_base(cwd=str(work))
    assert code == 0
    assert msg is None


def test_stale_branch_refused_with_fix(tmp_path):  # AC1-ERR
    work = _repo(tmp_path, timedelta(days=3))
    code, msg = _preflight.check_stale_base(cwd=str(work))
    assert code == 3
    assert msg is not None
    assert "days behind" in msg
    assert "phantom deletions" in msg
    assert "fno pr rebase --base=origin/main" in msg
    assert "FNO_PR_BASE_OK=stale-acknowledged" in msg


def test_fetch_failure_fails_open(tmp_path):  # AC2-ERR
    # A repo with an origin that points nowhere -> fetch fails -> fail-open.
    work = tmp_path / "work"
    subprocess.run(["git", "init", "-b", "main", str(work)], check=True)
    _git(work, "config", "user.email", "t@t.t")
    _git(work, "config", "user.name", "t")
    _commit(work, "base.txt", datetime.now(timezone.utc), "init")
    _git(work, "remote", "add", "origin", str(tmp_path / "does-not-exist.git"))
    code, msg = _preflight.check_stale_base(cwd=str(work))
    assert code == 0
    assert "skipped" in (msg or "")


def test_zero_behind_passes_regardless_of_age(tmp_path):  # AC1-EDGE
    now = datetime.now(timezone.utc)
    bare = tmp_path / "origin.git"
    subprocess.run(["git", "init", "--bare", "-b", "main", str(bare)], check=True)
    work = tmp_path / "work"
    subprocess.run(["git", "init", "-b", "main", str(work)], check=True)
    _git(work, "config", "user.email", "t@t.t")
    _git(work, "config", "user.name", "t")
    _commit(work, "base.txt", now - timedelta(days=8), "week-old but unchanged")
    _git(work, "remote", "add", "origin", str(bare))
    _git(work, "push", "-u", "origin", "main")
    _git(work, "checkout", "-b", "feature/x")  # sits at origin/main tip
    code, msg = _preflight.check_stale_base(cwd=str(work))
    assert code == 0
    assert msg is None


def test_no_merge_base_refuses_distinctly(tmp_path):  # AC2-EDGE
    bare = tmp_path / "origin.git"
    subprocess.run(["git", "init", "--bare", "-b", "main", str(bare)], check=True)
    work = tmp_path / "work"
    subprocess.run(["git", "init", "-b", "main", str(work)], check=True)
    _git(work, "config", "user.email", "t@t.t")
    _git(work, "config", "user.name", "t")
    _commit(work, "base.txt", datetime.now(timezone.utc), "init")
    _git(work, "remote", "add", "origin", str(bare))
    _git(work, "push", "-u", "origin", "main")
    # Orphan branch: no common ancestor with origin/main.
    _git(work, "checkout", "--orphan", "feature/orphan")
    _git(work, "rm", "-rf", ".")
    _commit(work, "other.txt", datetime.now(timezone.utc), "unrelated root")
    code, msg = _preflight.check_stale_base(cwd=str(work))
    assert code == 4
    assert "unrelated histories" in (msg or "")


def test_bypass_passes_and_emits_gate_escape(tmp_path):  # AC1-FR
    work = _repo(tmp_path, timedelta(days=3))  # genuinely stale
    events = tmp_path / "events.jsonl"
    env = {"FNO_PR_BASE_OK": "stale-acknowledged", "FNO_SESSION": "s1"}
    code, msg = _preflight.check_stale_base(
        cwd=str(work), env=env, events_path=events
    )
    assert code == 0
    assert msg is None
    lines = [json.loads(x) for x in events.read_text().splitlines() if x.strip()]
    escapes = [e for e in lines if e.get("type") == "gate_escape"]
    assert escapes, "bypass must emit a gate_escape event"
    assert escapes[-1]["data"]["reason"] == "stale-base"


def test_bare_ref_compares_remote_not_local(tmp_path):
    """A bare `--base main` must compare origin/main (the fetched ref), not the
    local main branch which git fetch never advances."""
    work = _repo(tmp_path, timedelta(days=3))  # HEAD=feature/x; origin/main fresh
    mb = subprocess.run(
        ["git", "merge-base", "HEAD", "origin/main"],
        cwd=work, text=True, capture_output=True, check=True,
    ).stdout.strip()
    # Rewind LOCAL main to the old merge-base -> local main is stale, origin fresh.
    _git(work, "branch", "-f", "main", mb)
    code, _ = _preflight.check_stale_base(base="main", cwd=str(work))
    assert code == 3  # would be 0 (pass) if it compared the stale local main


def test_slash_base_resolves_to_origin_not_phantom_remote(tmp_path):
    """A slash-containing base branch ('releases/v1') with no matching remote
    resolves to origin/releases/v1, not a phantom remote 'releases' whose fetch
    would fail and silently skip the guard."""
    now = datetime.now(timezone.utc)
    bare = tmp_path / "origin.git"
    subprocess.run(["git", "init", "--bare", "-b", "main", str(bare)], check=True)
    work = tmp_path / "work"
    subprocess.run(["git", "init", "-b", "main", str(work)], check=True)
    _git(work, "config", "user.email", "t@t.t")
    _git(work, "config", "user.name", "t")
    _commit(work, "base.txt", now - timedelta(days=3), "init")  # old merge-base
    _git(work, "remote", "add", "origin", str(bare))
    _git(work, "push", "-u", "origin", "main")
    _git(work, "checkout", "-b", "releases/v1")
    _git(work, "push", "-u", "origin", "releases/v1")
    _git(work, "checkout", "-b", "feature/x")
    _commit(work, "feature.txt", now, "feature work")
    _git(work, "checkout", "releases/v1")
    _commit(work, "release-advance.txt", now, "release advances")
    _git(work, "push", "origin", "releases/v1")
    _git(work, "checkout", "feature/x")
    code, msg = _preflight.check_stale_base(base="releases/v1", cwd=str(work))
    assert code == 3  # refused, not silently skipped by a failed fetch
    assert "days behind" in (msg or "")


def test_wrong_bypass_value_still_refuses(tmp_path):  # AC1-FR
    work = _repo(tmp_path, timedelta(days=3))
    code, _ = _preflight.check_stale_base(cwd=str(work), env={"FNO_PR_BASE_OK": "1"})
    assert code == 3


def test_emit_failure_never_blocks_bypass(tmp_path):  # AC2-FR
    work = _repo(tmp_path, timedelta(days=3))
    # events_path is a directory -> append fails internally; bypass still passes.
    bad = tmp_path / "adir"
    bad.mkdir()
    code, msg = _preflight.check_stale_base(
        cwd=str(work),
        env={"FNO_PR_BASE_OK": "stale-acknowledged", "FNO_SESSION": "s2"},
        events_path=bad,
    )
    assert code == 0
    assert msg is None
