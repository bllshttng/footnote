"""Every gh spawn `fno do pr status` makes, counted at the subprocess layer.

The fake sits one level BELOW the verbs, at `fno.pr._proc.subprocess.run`, so
an absolute gh path (the GraphQL broker's resolve_real_gh spawn) counts
exactly like a bare `gh` - the blind spot that left 4 of 20 spawns on a red
read uncounted. The bounds it pins, on fixture F6 (a red head, 6
failing Actions checks with distinct job ids, 28 passing checks on one page,
logs with no smoke-runner lines, and the review, coverage, hold and lane
probes stubbed):

- a cache miss spends at most 15 spawns (2 PR reads, 1 check-runs page, 1
  runs listing, 1 combined status, 5 logs, 5 job objects), plus 5
  fno-agents cause reads that are counted as their own class (the binary is
  a subprocess spawn, never a gh one, and its internal gh reads ride the
  op's own bounds);
- a second read inside the TTL spends exactly 1, the head read;
- a same-head refresh past the TTL spends at most 5 with 0 log and 0
  job-object reads - failure detail reused by job id;
- a re-run that replaced a job id fetches only the NEW job's log;
- a pushed head reuses nothing.

And on a green head: the runs listing is read once per miss (not twice), and
a same-head refresh at an equal check count reuses the rerun facts (0
attempts and 0 jobs reads). No test here touches the network: the fake gh is
the proof (the fake gh is the proof; no live read verifies this).
"""
from __future__ import annotations

import json
import re
import time
from types import SimpleNamespace

import pytest

from fno.pr import _cache, _merge, _proc, _quota, _rest, _status, _wait


# --- the fake gh ------------------------------------------------------------


class _FakeGh:
    """subprocess.run stand-in answering the REST endpoints status reads."""

    def __init__(self):
        self.world: dict = {}
        self.argvs: list[list[str]] = []

    def __call__(self, cmd, **kwargs):
        argv = [str(a) for a in cmd]
        self.argvs.append(argv)
        # The fno-agents binary is a subprocess spawn but not a gh spawn:
        # answer its op receipts so the read behaves as it would against the
        # real binary, and let the classifier count the class separately.
        if argv[0].endswith("fno-agents"):
            return self._agents_answer(argv, kwargs)
        path = next((a for a in argv[2:] if not a.startswith("-")), "")
        if re.search(r"/pulls/\d+$", path):
            return self._json(self.world["pr"])
        if "/check-runs" in path:
            return self._json(
                {"total_count": len(self.world["check_runs"]), "check_runs": self.world["check_runs"]}
            )
        if "/actions/runs?head_sha=" in path:
            return self._json({"total_count": len(self.world["runs"]), "workflow_runs": self.world["runs"]})
        if path.endswith("/status"):
            return self._json({"state": "success", "statuses": []})
        m = re.search(r"/actions/jobs/(\d+)/logs$", path)
        if m:
            log = self.world["jobs"][m.group(1)][0]
            return SimpleNamespace(returncode=0, stdout=log, stderr="")
        m = re.search(r"/actions/jobs/(\d+)$", path)
        if m:
            return self._json({"steps": self.world["jobs"][m.group(1)][1]})
        m = re.search(r"/actions/runs/(\d+)/attempts\?", path)
        if m:
            return self._json({"workflow_runs": self.world["attempts"]})
        if "/attempts/" in path and path.endswith("/jobs?per_page=100"):
            return self._json({"jobs": self.world["attempt_jobs"], "total_count": 1})
        return SimpleNamespace(returncode=1, stdout="", stderr=f"unexpected argv: {path}")

    def _json(self, payload) -> SimpleNamespace:
        return SimpleNamespace(returncode=0, stdout=json.dumps(payload), stderr="")

    def _agents_answer(self, argv, kwargs) -> SimpleNamespace:
        if argv[-1] != "authorized-merge":
            return SimpleNamespace(returncode=1, stdout="", stderr=f"unexpected verb: {argv}")
        try:
            payload = json.loads(kwargs.get("input") or "{}")
        except json.JSONDecodeError:
            return SimpleNamespace(returncode=2, stdout="", stderr="bad payload")
        op = payload.get("op")
        if op == "status-failure-cause":
            return self._json({"items": [{"cause": None}] * len(payload.get("items") or [])})
        if op == "status-merge-blocker":
            return self._json(
                {"state": None, "blockers": [], "missing_required_checks": None, "source": "fake"}
            )
        return SimpleNamespace(returncode=2, stdout="", stderr=f"unexpected op: {op}")

    def since(self, index: int) -> list[list[str]]:
        return self.argvs[index:]


def _classes(argvs: list[list[str]]) -> dict:
    c: dict = {
        "pulls": 0,
        "checks": 0,
        "runs": 0,
        "status": 0,
        "logs": [],
        "jobs": [],
        "attempts": 0,
        "attempt_jobs": 0,
        "agents": [],
        "other": [],
    }
    for cmd in argvs:
        # The fno-agents binary: a subprocess spawn, never a gh one. Its
        # internal gh reads ride the op's own bounds and are invisible here -
        # the class exists so a binary spawn can never masquerade as
        # unclassified gh spend.
        if cmd[0].endswith("fno-agents"):
            c["agents"].append(cmd[-1] if cmd[1:] else "")
            continue
        path = next((a for a in cmd[2:] if not a.startswith("-")), "")
        if re.search(r"/pulls/\d+$", path):
            c["pulls"] += 1
        elif "/check-runs" in path:
            c["checks"] += 1
        elif "/actions/runs?head_sha=" in path:
            c["runs"] += 1
        elif path.endswith("/status"):
            c["status"] += 1
        elif re.search(r"/actions/jobs/(\d+)/logs$", path):
            c["logs"].append(re.search(r"/actions/jobs/(\d+)/logs$", path).group(1))
        elif re.search(r"/actions/jobs/(\d+)$", path):
            c["jobs"].append(re.search(r"/actions/jobs/(\d+)$", path).group(1))
        elif re.search(r"/actions/runs/\d+/attempts\?", path):
            c["attempts"] += 1
        elif "/attempts/" in path and "/jobs" in path:
            c["attempt_jobs"] += 1
        else:
            c["other"].append(path)
    return c


# --- fixtures ---------------------------------------------------------------


def _f6_world(sha: str) -> dict:
    """F6: a red head, 6 failing Actions checks (job ids 901-906), 28 passing."""
    checks: list[dict] = []
    jobs: dict[str, tuple[str, list[dict]]] = {}
    for i in range(1, 7):
        job_id = str(900 + i)
        checks.append(
            {
                "name": f"guard-{i}",
                "status": "completed",
                "conclusion": "failure",
                "started_at": "2026-09-15T14:00:00Z",
                "details_url": f"https://github.com/owner/repo/actions/runs/700/job/{job_id}",
            }
        )
        jobs[job_id] = (
            "Build output line one\nBuild output line two\n",
            [
                {"name": "Set up job", "conclusion": "success"},
                {"name": "Build", "conclusion": "failure"},
                {"name": "Test", "conclusion": "skipped"},
                {"name": "Post Checkout", "conclusion": "success"},
            ],
        )
    for i in range(28):
        checks.append(
            {
                "name": f"passing-{i}",
                "status": "completed",
                "conclusion": "success",
                "started_at": "2026-09-15T14:00:00Z",
                "details_url": f"https://github.com/owner/repo/actions/runs/700/job/{2000 + i}",
            }
        )
    return {
        "pr": _pr_payload(sha),
        "check_runs": checks,
        "runs": [{"id": 700, "name": "guards", "status": "completed", "conclusion": "failure", "run_attempt": 1}],
        "jobs": jobs,
        "attempts": [],
        "attempt_jobs": [],
    }


def _green_world(sha: str) -> dict:
    """A green head whose 12 workflow runs are all second attempts."""
    return {
        "pr": _pr_payload(sha),
        "check_runs": [
            {
                "name": "ci",
                "status": "completed",
                "conclusion": "success",
                "started_at": "2026-09-15T14:00:00Z",
                "details_url": "https://github.com/owner/repo/actions/runs/800/job/300",
            }
        ],
        "runs": [
            {"id": 800 + i, "name": f"wf-{i}", "status": "completed", "conclusion": "success", "run_attempt": 2}
            for i in range(12)
        ],
        "jobs": {"300": ("all green\n", [{"name": "Test", "conclusion": "success"}])},
        "attempts": [{"id": 1, "run_attempt": 1, "conclusion": "failure"}],
        "attempt_jobs": [{"name": "Test", "conclusion": "failure"}],
    }


def _pr_payload(sha: str) -> dict:
    return {
        "number": 42,
        "state": "open",
        "merged_at": None,
        "body": "",
        "html_url": None,
        "merge_commit_sha": None,
        "auto_merge": None,
        "user": {"login": "someone"},
        "head": {"sha": sha, "ref": "feature/x"},
        "base": {"sha": "b" * 40, "ref": "main"},
        "mergeable": True,
    }


@pytest.fixture
def gh(monkeypatch):
    fake = _FakeGh()
    monkeypatch.setattr(_proc.subprocess, "run", fake)
    return fake


@pytest.fixture(autouse=True)
def env(tmp_path, monkeypatch):
    monkeypatch.setenv("FNO_PR_STATUS_CACHE_DIR", str(tmp_path / "cache"))
    monkeypatch.setattr(_rest, "_repo_slug", lambda cwd=None: "owner/repo")
    monkeypatch.setattr(
        _rest, "_slug_or_reason", lambda cwd=None, runner=None, repo=None: ("owner/repo", "")
    )
    # The fleet-ledger probes: stubbed so no test opens a live budget read.
    monkeypatch.setattr(_quota, "backoff_live", lambda: False)
    monkeypatch.setattr(_quota, "record_refusal", lambda stderr: None)
    monkeypatch.setattr(_quota, "admit", lambda argv: None)
    # The review, coverage, hold and lane probes are not under test.
    monkeypatch.setattr(
        _status,
        "read_optional_review_state",
        lambda pr, cwd: {
            "optional_reviews": [],
            "optional_reviews_unresolved": 0,
            "optional_reviews_resolved_unchanged": 0,
        },
    )
    monkeypatch.setattr(
        _status, "read_review_coverage", lambda pr, cwd, **kw: {"coverage": "unknown", "reviewed_count": None}
    )
    monkeypatch.setattr(_status, "_merge_hold_reason", lambda pr, cwd: None)
    monkeypatch.setattr(_status, "_review_lane", lambda pr, cwd: False)
    monkeypatch.setattr(
        _status,
        "_review_activity",
        lambda branch, head, cwd: SimpleNamespace(blocked=False, blocker=None, detail="", hold=None, worktree=None),
    )
    monkeypatch.setattr(_status, "_merge_authority", lambda repo: {"may_merge": False})
    monkeypatch.setattr(_status, "_merge_execution_projection", lambda repo, pr: {"state": "absent"})
    monkeypatch.setattr(_merge, "_code_review_attestation_required", lambda repo, pr: False)


def _age_row(sha: str, seconds: int = 999) -> None:
    key = f"owner--repo-42-{sha[:12]}"
    p = _cache.cache_dir() / f"{key}.json"
    row = json.loads(p.read_text())
    row["ts"] = time.time() - seconds
    p.write_text(json.dumps(row))


# --- the counter (change 1) -------------------------------------------------


def test_counter_counts_an_absolute_gh_path(monkeypatch):
    """`_proc.run` counts by basename: the GraphQL broker's absolute gh spawn
    spent 4 uncounted reads per status read before this."""
    monkeypatch.setattr(
        _proc.subprocess, "run", lambda cmd, **kw: SimpleNamespace(returncode=0, stdout="{}", stderr="")
    )
    before = _proc.GH_CALLS
    _proc.run(["/opt/homebrew/bin/gh", "api", "rate_limit"])
    assert _proc.GH_CALLS - before == 1
    before = _proc.GH_CALLS
    _proc.run(["git", "status"])
    assert _proc.GH_CALLS - before == 0


def test_wait_note_names_the_tick_count(monkeypatch, capsys):
    """A wait that polled 3 times says `over 3 status read(s)`: a note that
    summed a whole wait read as one read's spend and filed this node."""
    tick = {"n": 0}

    def fake_cached(pr, cwd=None, refresh=False):
        tick["n"] += 1
        if tick["n"] < 3:
            print(json.dumps({"settled": False, "verdict": "pending"}))
            return 2
        print(json.dumps({"settled": True, "green": True, "verdict": "green"}))
        return 0

    monkeypatch.setattr(_cache, "cached_status", fake_cached)
    t = {"now": 0.0}
    rc = _wait.wait_status(
        "42",
        until="settled",
        timeout=600,
        interval=60,
        sleeper=lambda s: t.__setitem__("now", t["now"] + s),
        clock=lambda: t["now"],
    )
    assert rc == 0
    assert "over 3 status read(s)" in capsys.readouterr().err


def test_review_note_names_the_tick_count(monkeypatch, capsys):
    counts = iter([5, 5, 7])
    monkeypatch.setattr(_wait, "_review_count", lambda pr, cwd, slug="": next(counts))
    import fno.pr._base_lineage as _lineage

    monkeypatch.setattr(_lineage, "_repo_slug", lambda cwd=None: "owner/repo")
    t = {"now": 0.0}
    rc = _wait.wait_status(
        "42",
        until="review",
        timeout=600,
        interval=60,
        sleeper=lambda s: t.__setitem__("now", t["now"] + s),
        clock=lambda: t["now"],
    )
    assert rc == 0
    assert "over 3 review read(s)" in capsys.readouterr().err


def test_status_main_appends_fleet_budget(monkeypatch, capsys, tmp_path):
    """When the ledger answered, the spend note carries the budget beside the
    count; when backoff_live never ran (--refresh), it stays silent."""
    monkeypatch.setenv("FNO_PR_STATUS_CACHE_DIR", str(tmp_path / "cache"))
    monkeypatch.setattr(_rest, "_repo_slug", lambda cwd=None: "owner/repo")
    monkeypatch.setattr(_proc.subprocess, "run", lambda cmd, **kw: SimpleNamespace(returncode=1, stdout="", stderr="HTTP 500: oops"))
    monkeypatch.setattr(_quota, "backoff_live", lambda: False)
    monkeypatch.setattr(_quota, "LAST_BUDGET", {"points_60s": 9, "cap": 500})
    monkeypatch.setattr(_status, "_fetch", lambda pr, cwd: (None, "boom"))
    assert _status.main(["42"]) == 4
    assert "fleet budget 9 of 500 points in the last 60s" in capsys.readouterr().err

    monkeypatch.setattr(_quota, "LAST_BUDGET", None)
    assert _status.main(["42", "--refresh"]) == 4
    assert "fleet budget" not in capsys.readouterr().err


def test_budget_note_guard(monkeypatch):
    monkeypatch.setattr(_quota, "LAST_BUDGET", {"points_60s": "many", "cap": 500})
    assert _quota.budget_note() == ""


# --- F6 bounds (changes 1 and 2) ---------------------------------------------


def test_f6_miss_sends_at_most_15_spawns(gh, capsys):
    gh.world = _f6_world("c" * 40)
    assert _cache.cached_status("42") == 1
    c = _classes(gh.argvs)
    assert c["pulls"] == 2 and c["checks"] == 1 and c["runs"] == 1 and c["status"] == 1
    assert len(c["logs"]) == 5 and len(c["jobs"]) == 5
    assert not c["other"]
    # One fno-agents cause read per detailed failure (MAX_DETAILED_FAILURES),
    # not a gh spawn: the class is bounded, never unclassified.
    assert len(c["agents"]) == 5
    assert json.loads(capsys.readouterr().out)["verdict"] == "red"


def test_f6_second_read_inside_ttl_spends_exactly_one(gh, capsys):
    gh.world = _f6_world("c" * 40)
    assert _cache.cached_status("42") == 1
    capsys.readouterr()
    before = len(gh.argvs)
    assert _cache.cached_status("42") == 1
    calls = _classes(gh.since(before))
    assert len(gh.since(before)) == 1, "the head read is the only spawn"
    assert calls["pulls"] == 1
    capsys.readouterr()


def test_f6_same_head_refresh_reuses_failure_detail_by_job_id(gh, capsys):
    gh.world = _f6_world("c" * 40)
    assert _cache.cached_status("42") == 1
    first = json.loads(capsys.readouterr().out)
    _age_row("c" * 40)
    before = len(gh.argvs)
    assert _cache.cached_status("42") == 1
    calls = _classes(gh.since(before))
    assert len(calls["logs"]) == 0 and len(calls["jobs"]) == 0
    assert len(gh.since(before)) <= 5
    second = json.loads(capsys.readouterr().out)
    assert second["failures"] == first["failures"]


def test_f6_rerun_fetches_only_the_new_job_log(gh, capsys):
    gh.world = _f6_world("c" * 40)
    assert _cache.cached_status("42") == 1
    capsys.readouterr()
    world = _f6_world("c" * 40)
    for check in world["check_runs"]:
        if check["name"] == "guard-3":
            check["details_url"] = "https://github.com/owner/repo/actions/runs/701/job/913"
    world["jobs"]["913"] = (
        "smoke: fail   8s  Build\nsmoke: step failed, stopping (fail-fast): Build\n",
        [],
    )
    del world["jobs"]["903"]
    gh.world = world
    _age_row("c" * 40)
    before = len(gh.argvs)
    assert _cache.cached_status("42") == 1
    calls = _classes(gh.since(before))
    assert calls["logs"] == ["913"]
    capsys.readouterr()


def test_f6_pushed_head_reuses_nothing(gh, capsys):
    gh.world = _f6_world("c" * 40)
    assert _cache.cached_status("42") == 1
    capsys.readouterr()
    gh.world = _f6_world("d" * 40)
    before = len(gh.argvs)
    assert _cache.cached_status("42") == 1
    payload = json.loads(capsys.readouterr().out)
    assert payload["head"] == "d" * 40
    calls = _classes(gh.since(before))
    assert len(calls["logs"]) == 5, "a new head fetches its own detail"


# --- green head (change 3) ---------------------------------------------------


def test_green_miss_reads_the_runs_listing_once(gh, capsys):
    gh.world = _green_world("e" * 40)
    assert _cache.cached_status("42") == 0
    calls = _classes(gh.argvs)
    assert calls["runs"] == 1, "rerun_recovery reuses the listing fetch_pr_rest read"
    payload = json.loads(capsys.readouterr().out)
    assert payload["verdict"] == "green"
    assert payload["rerun_recovered"] is True


def test_green_refresh_reuses_the_rerun_facts(gh, capsys):
    gh.world = _green_world("e" * 40)
    assert _cache.cached_status("42") == 0
    first = json.loads(capsys.readouterr().out)
    _age_row("e" * 40)
    before = len(gh.argvs)
    assert _cache.cached_status("42") == 0
    calls = _classes(gh.since(before))
    assert calls["attempts"] == 0 and calls["attempt_jobs"] == 0
    second = json.loads(capsys.readouterr().out)
    assert second["rerun_recovered"] == first["rerun_recovered"]
    assert second["recovered_failures"] == first["recovered_failures"]
