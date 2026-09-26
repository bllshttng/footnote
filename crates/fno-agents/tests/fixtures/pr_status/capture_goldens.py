"""Capture goldens from the Python pr-status leg before its deletion.

One fixture per scenario in crates/fno-agents/tests/fixtures/pr_status/.
Each fixture carries the RAW gh responses (the fake runner's url->payload
table), the assembled pr_json the real fetch_pr_rest produced, the canned
seam values, and the Python leg's exact (stdout, stderr, exit). The Rust
port replays raw through read_pr and asserts assembly, verdict, counts,
and the composed output byte for byte (port protocol,
docs/architecture/dual-implementation-inventory.md).

Run from the worktree root:
  /Users/bb16/code/footnote/footnote/cli/.venv/bin/python \\
    /Users/bb16/.claude/jobs/29f4c798/tmp/capture_goldens.py
"""

from __future__ import annotations

import io
import json
import sys
import time
from contextlib import ExitStack, redirect_stderr, redirect_stdout
from pathlib import Path
from unittest import mock

REPO = Path("/Users/bb16/.fno/worktrees/footnote/x-8ab0")
sys.path.insert(0, str(REPO / "cli" / "src"))

OUT_DIR = REPO / "crates" / "fno-agents" / "tests" / "fixtures" / "pr_status"

NOW = 1790409600  # frozen clock; cached_at/cached_age_seconds derive from it
HEAD = "b41ac4bfeedface0123456789abcdef012345678"

ALLOWED_KEYS = {
    "pr", "head", "verdict", "settled", "green", "pr_state", "mergeable",
    "github_merge_state", "checks", "failures", "optional_reviews",
    "optional_reviews_unresolved", "optional_reviews_resolved_unchanged",
    "review_coverage", "review_posture", "merge_authority", "merge_execution",
    "rounds_used", "max_rounds", "rounds_exhausted", "review_activity",
    "dispatch_hold", "ready", "ready_blockers", "cached", "cached_at",
    "cached_age_seconds", "rerun_recovered", "recovered_failures",
    "coverage_waiver", "review_owner_guidance", "coverage_status_repost",
    "rounds_note", "stale_verdict", "stale_reason", "rate_limit_class",
    "reason", "merge_decision", "branch_history", "reader_error",
}


class _R:
    """A fake gh Result."""

    def __init__(self, ok, stdout, stderr=""):
        self.ok, self.stdout, self.stderr = ok, stdout, stderr


class _Reason(str):
    rate_limit_class = ""


def raw_check(name, status="completed", conclusion="success", job_id=None, started="2026-09-25T00:00:00Z"):
    return {
        "name": name,
        "status": status,
        "conclusion": conclusion,
        "started_at": started,
        "details_url": f"https://github.com/Owner/Repo/actions/runs/1/job/{job_id or name}",
    }


def raw_status(context, state):
    return {"context": context, "state": state, "created_at": "2026-09-25T00:00:00Z", "target_url": "https://ci.example/1"}


LOG_TEXT = (
    "2026-09-25T00:00:01Z ##[group]Run smoke\n"
    "2026-09-25T00:00:02Z ##[endgroup]\n"
    "2026-09-25T00:00:03Z Current runner version: 2.311.0\n"
    "2026-09-25T00:00:04Z ##[error]Process completed with exit code 1.\n"
    "2026-09-25T00:00:05Z Cleaning up orphan processes\n"
)

LOG_TEXT_PLAIN = (
    "2026-09-25T00:00:01Z step one ok\n"
    "2026-09-25T00:00:02Z assert_eq failed: expected 200, got 401\n"
    "2026-09-25T00:00:03Z ##[error]Process completed with exit code 1.\n"
)


def base():
    checks = [raw_check("ci", job_id=1001), raw_check("lint", job_id=1002)]
    return {
        "pulls": {
            "state": "open", "merged": False,
            "mergeable": True, "merge_state_status": "clean",
            "head": {"sha": HEAD, "ref": "feature/x-8ab0"},
            "base": {"ref": "main"},
        },
        "check_runs_pages": [{"total_count": len(checks), "check_runs": checks}],
        "runs_listing": {"total_count": 0, "workflow_runs": []},
        "zero_rows": [],
        "statuses": {"state": "success", "statuses": []},
        "job_logs": {"1001": LOG_TEXT, "1002": LOG_TEXT_PLAIN},
        "job_steps": {},
        "failed_job_logs": {},
        "fetch_stderr": "",
        "fetch_fail": False,
        "rerun_recovery": {"recovered": False, "failed": []},
        "branch_history": None,
        "optional_reviews": {"optional_reviews": [], "optional_reviews_unresolved": 0, "optional_reviews_resolved_unchanged": 0},
        "coverage_row": {"coverage": "not_asked", "reviewed_count": None},
        "review_lane": True,
        "hold_reason": None,
        "review_activity": {
            "blocker": "", "detail": "", "hold": None,
            "worktree": {"probed": False, "path": None, "dirty": None, "head": None, "note": "not asked: PR is terminal"},
        },
        "receipt": {"outcome": "held", "head": HEAD, "blockers": [{"code": "ci_green", "class": "held", "detail": "CI is green; awaiting review coverage"}]},
        "github_merge_blockers": None,
        "merge_authority": {"config_auto_merge_enabled": True, "grant": "dispatch"},
        "merge_execution": None,
        "cache": None,
    }


def apply_and_capture(name, s, *, use_cache=False, refresh=False):
    from fno.pr import _cache, _failures, _merge, _quota, _rest, _review_hold, _status

    table = {
        "repos/Owner/Repo/pulls/42": (not s["fetch_fail"], json.dumps(s["pulls"]), s["fetch_stderr"]),
        "commits/" + HEAD + "/check-runs": (True, json.dumps({"total_count": 0, "check_runs": []}), ""),
    }
    # check-runs pages: the fake matches on the path prefix; serve pages in order.
    pages = s["check_runs_pages"]

    def fake_runner(args, cwd=None, **kw):
        cmd = " ".join(args)
        if "/pulls/42" in cmd:
            ok, out, err = table["repos/Owner/Repo/pulls/42"]
            return _R(ok, out, err)
        if "/check-runs" in cmd and "page=" in cmd:
            page = int(cmd.rsplit("page=", 1)[1])
            body = pages[page - 1] if page <= len(pages) else {"total_count": 0, "check_runs": []}
            return _R(True, json.dumps(body))
        if "/actions/runs?head_sha=" in cmd:
            return _R(True, json.dumps(s["runs_listing"]))
        if cmd.endswith("/status"):
            return _R(True, json.dumps(s["statuses"]))
        if "/actions/jobs/" in cmd and cmd.endswith("/logs"):
            job = cmd.rsplit("/jobs/", 1)[1].split("/")[0]
            if job in s["failed_job_logs"]:
                return _R(False, "", s["failed_job_logs"][job])
            return _R(True, s["job_logs"].get(job, ""))
        if "/actions/jobs/" in cmd:
            job = cmd.rsplit("/jobs/", 1)[1].split("?")[0]
            return _R(True, json.dumps({"steps": s["job_steps"].get(job, [])}))
        if "runs/1" in cmd or "/actions/runs/" in cmd:
            return _R(True, json.dumps({"total_count": 0, "workflow_runs": [], "jobs": []}))
        raise AssertionError(f"capture runner has no answer for: {cmd}")

    def _fetch(pr, cwd):
        from fno.pr._rest import fetch_pr_rest

        if s["fetch_fail"]:
            reason = _Reason(s["fetch_stderr"])
            reason.rate_limit_class = s.get("rate_limit_class", "")
            return None, reason
        return fetch_pr_rest(pr, cwd, runner=fake_runner)

    def fetch_job_log(owner, repo, job_id, cwd, runner=None):
        res = fake_runner(["gh", "api", f"repos/{owner}/{repo}/actions/jobs/{job_id}/logs"], cwd)
        return res

    def fetch_job_steps(owner, repo, job_id, cwd, runner=None):
        return s["job_steps"].get(str(job_id), [])

    def _merge_decision(pr, repo, facts):
        return s["receipt"]

    activity = _review_hold.ReviewActivity(
        False,
        s["review_activity"]["blocker"], s["review_activity"]["detail"],
        s["review_activity"]["hold"],
        s["review_activity"]["worktree"],
    )

    captured = {}
    real_run_status = _status.run_status

    def run_status_spy(pr, cwd=None, **kw):
        code = real_run_status(pr, cwd, **kw)
        captured["pr_json"] = kw.get("prior") and None
        return code

    patches = [
        mock.patch.object(_status, "_fetch", _fetch),
        mock.patch.object(_status, "rerun_recovery", lambda pr, cwd=None, sha=None, runs=None: s["rerun_recovery"]),
        mock.patch.object(_status, "_branch_history", lambda pr_json, rollup, cwd, prior: s["branch_history"]),
        mock.patch.object(_status, "read_optional_review_state", lambda pr, cwd: s["optional_reviews"]),
        mock.patch.object(_status, "read_review_coverage", lambda pr, cwd, **kw: s["coverage_row"]),
        mock.patch.object(_status, "_review_lane", lambda pr, cwd: s["review_lane"]),
        mock.patch.object(_status, "_merge_hold_reason", lambda pr, cwd: s["hold_reason"]),
        mock.patch.object(_status, "_review_activity", lambda branch, head, cwd: activity),
        mock.patch.object(_status, "_merge_decision", _merge_decision),
        mock.patch.object(_status, "_merge_authority", lambda repo: s["merge_authority"]),
        mock.patch.object(_status, "_merge_execution_projection", lambda repo, pr: s["merge_execution"]),
        mock.patch.object(_merge, "_code_review_attestation_required", lambda repo, pr_number=0: False),
        mock.patch.object(_failures, "fetch_job_log", fetch_job_log),
        mock.patch.object(_failures, "_fetch_job_steps", fetch_job_steps),
        mock.patch.object(_quota, "budget_note", lambda: ""),
        mock.patch.object(_quota, "backoff_live", lambda: (s["cache"] or {}).get("backoff_live", False)),
        mock.patch.object(_rest, "_repo_slug", lambda cwd=None, runner=None: "Owner/Repo"),
        mock.patch.object(_rest, "_repo_slug_reason", lambda cwd=None, runner=None, repo=None: ("Owner/Repo", "")),
        mock.patch.object(_rest, "_zero_job_rows", lambda slug, cwd, sha, check_runs: (s["zero_rows"], s["runs_listing"]["workflow_runs"], "")),
        mock.patch.object(_rest, "fetch_pr_info_rest", lambda pr, cwd=None, **kw: ({"head_sha": HEAD, "state": s["pulls"].get("state", "open").upper(), "head_ref": "feature/x-8ab0", "mergeable": "MERGEABLE", "merge_state_status": "clean", "base_ref": "main"}, "")),
        mock.patch.object(_cache, "_merge_decision_key", lambda slug_key, pr, info, cwd: f"{slug_key}-{pr}-{info['head_sha'][:12]}"),
        mock.patch("time.time", lambda: NOW),
    ]

    import os
    import tempfile

    with ExitStack() as stack:
        for p in patches:
            stack.enter_context(p)
        if use_cache:
            tmp = tempfile.mkdtemp(prefix="golden-cache-")
            os.environ["FNO_PR_STATUS_CACHE_DIR"] = tmp
            for key, row in (s["cache"] or {}).get("seed_rows", {}).items():
                Path(tmp, f"{key}.json").write_text(json.dumps(row))
            out, err = io.StringIO(), io.StringIO()
            with redirect_stdout(out), redirect_stderr(err):
                code = _cache.cached_status("42", refresh=refresh)
            return code, out.getvalue(), err.getvalue(), s
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            code = _status.run_status("42")
        return code, out.getvalue(), err.getvalue(), s


SCENARIOS = []


def scenario(name, **kw):
    def wrap(fn):
        SCENARIOS.append((name, fn, kw))
        return fn
    return wrap


@scenario("green_settled")
def _green(s):
    pass


@scenario("red_detailed_capped")
def _red(s):
    checks = [
        raw_check("smoke", "completed", "failure", 2001),
        raw_check("pytest", "completed", "failure", 2002),
        raw_check("lint", "completed", "failure", 2003),
        raw_check("mypy", "completed", "failure", 2004),
        raw_check("fmt", "completed", "failure", 2005),
        raw_check("docs", "completed", "failure", 2006),
    ]
    s["check_runs_pages"] = [{"total_count": len(checks), "check_runs": checks}]
    s["job_logs"] = {"2001": LOG_TEXT, "2002": LOG_TEXT_PLAIN}
    s["failed_job_logs"] = {"2003": "gh: exit 4, secondary rate limit"}
    s["receipt"] = {"outcome": "held", "head": HEAD, "blockers": [{"code": "ci_red", "class": "held", "detail": "CI is red"}]}


@scenario("pending_mixed")
def _pending(s):
    checks = [
        raw_check("ci", "in_progress", "", 3001),
        raw_check("guard", "completed", "cancelled", 3002),
    ]
    s["check_runs_pages"] = [{"total_count": len(checks), "check_runs": checks}]
    s["receipt"] = {"outcome": "held", "head": HEAD, "blockers": [{"code": "ci_pending", "class": "held", "detail": "CI is unsettled"}]}


@scenario("all_status_contexts")
def _all_status(s):
    s["check_runs_pages"] = [{"total_count": 0, "check_runs": []}]
    s["statuses"] = {
        "state": "success",
        "statuses": [raw_status("ci/commit-status", "SUCCESS"), raw_status("docs/build", "SUCCESS")],
    }
    s["receipt"] = {"outcome": "held", "head": HEAD, "blockers": [{"code": "ci_green", "class": "held", "detail": "no real check runs"}]}


@scenario("zero_job_runs")
def _zero_job(s):
    s["check_runs_pages"] = [{"total_count": 0, "check_runs": []}]
    s["runs_listing"] = {
        "total_count": 1,
        "workflow_runs": [{
            "id": 9001, "conclusion": "failure", "status": "completed",
            "head_sha": HEAD, "name": "ci", "path": ".github/workflows/ci.yml",
            "html_url": "https://github.com/Owner/Repo/actions/runs/9001",
            "created_at": "2026-09-25T00:00:00Z",
        }],
    }
    # The row zero_job_runs_op mints for that listing: a completed failure no
    # check run links and no jobs behind (the Rust rule's own tests pin it).
    s["zero_rows"] = [{
        "name": ".github/workflows/ci.yml",
        "status": "completed",
        "conclusion": "failure",
        "startedAt": "2026-09-25T00:00:00Z",
        "detailsUrl": "https://github.com/Owner/Repo/actions/runs/9001",
        "workflow": ".github/workflows/ci.yml",
    }]
    s["receipt"] = {"outcome": "held", "head": HEAD, "blockers": [{"code": "ci_red", "class": "held", "detail": "workflow failed before minting a job"}]}


@scenario("terminal_merged")
def _terminal(s):
    s["pulls"] = dict(s["pulls"], state="closed", merged=True)
    s["receipt"] = {"outcome": "held", "head": HEAD, "blockers": [{"code": "pr_terminal", "class": "held", "detail": "PR 42 is already merged; nothing to merge"}]}


@scenario("rest_refusal_rate_limit")
def _refusal(s):
    s["fetch_fail"] = True
    s["fetch_stderr"] = "gh api pull request read: secondary rate limit"
    s["rate_limit_class"] = "secondary"
    s["receipt"] = {}


def _canned_failures(job_ids):
    return [
        {"check": f"check-{j}", "job_id": j, "step": "Run smoke", "first_error": "Process completed with exit code 1."}
        for j in job_ids
    ]


@scenario("stale_row_under_backoff", use_cache=True)
def _stale_backoff(s):
    checks = [raw_check("ci", job_id=1001)]
    s["check_runs_pages"] = [{"total_count": len(checks), "check_runs": checks}]
    green = {
        "pr": "42", "head": HEAD, "verdict": "green", "settled": True, "green": True,
        "pr_state": "OPEN", "mergeable": "MERGEABLE", "checks": {"total": 1},
        "failures": [{"check": "old", "first_error": "stale detail must drop"}],
    }
    s["cache"] = {
        "backoff_live": True,
        "seed_rows": {
            f"Owner--Repo-42-{HEAD[:12]}": {"ts": NOW - 600, "exit": 0, "output": green},
        },
    }


@scenario("refresh_flag", use_cache=True, refresh=True)
def _refresh(s):
    s["rerun_recovery"] = {"recovered": True, "failed": ["ci"]}
    old = {
        "pr": "42", "head": HEAD, "verdict": "pending", "settled": False, "green": False,
        "pr_state": "OPEN", "mergeable": "MERGEABLE", "checks": {"total": 1},
    }
    s["cache"] = {
        "seed_rows": {
            f"Owner--Repo-42-{HEAD[:12]}": {"ts": NOW - 600, "exit": 2, "output": old},
        },
    }


@scenario("prior_row_replays_failures", use_cache=True)
def _prior_replay(s):
    checks = [
        raw_check("smoke", "completed", "failure", 2001),
        raw_check("pytest", "completed", "failure", 2002),
    ]
    s["check_runs_pages"] = [{"total_count": len(checks), "check_runs": checks}]
    s["job_logs"] = {}
    s["receipt"] = {"outcome": "held", "head": HEAD, "blockers": [{"code": "ci_red", "class": "held", "detail": "CI is red"}]}
    prior = {
        "pr": "42", "head": HEAD, "verdict": "red", "settled": True, "green": False,
        "pr_state": "OPEN", "mergeable": "MERGEABLE",
        "checks": {"total": 2, "check_runs": 2, "fail": 2},
        "failures": _canned_failures(["2001", "2002"]),
    }
    s["cache"] = {
        "seed_rows": {
            f"Owner--Repo-42-{HEAD[:12]}": {"ts": NOW - 600, "exit": 1, "output": prior},
        },
    }


def main():
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    written = []
    for name, fn, kw in SCENARIOS:
        s = base()
        fn(s)
        code, out, err, _ = apply_and_capture(name, s, use_cache=kw.get("use_cache", False), refresh=kw.get("refresh", False))
        fixture = {
            "name": name,
            "now": NOW,
            "use_cache": kw.get("use_cache", False),
            "refresh": kw.get("refresh", False),
            "inputs": _inputs_of(s),
            "expected": {"exit": code, "stdout": out, "stderr_lines": err.splitlines()},
        }
        _check_keys(fixture)
        path = OUT_DIR / f"{name}.json"
        path.write_text(json.dumps(fixture, indent=2, sort_keys=True) + "\n")
        written.append(path.name)
    print(f"captured {len(written)} goldens: {written}")


def _inputs_of(s):
    """Everything a Rust replay must inject or serve, verbatim."""
    return {
        "raw": {
            "pulls": s["pulls"],
            "check_runs_pages": s["check_runs_pages"],
            "runs_listing": s["runs_listing"],
            "zero_rows": s["zero_rows"],
            "statuses": s["statuses"],
            "job_logs": s["job_logs"],
            "job_steps": s["job_steps"],
            "failed_job_logs": s["failed_job_logs"],
        },
        "fetch_fail": s["fetch_fail"],
        "fetch_stderr": s["fetch_stderr"],
        "rate_limit_class": s.get("rate_limit_class", ""),
        "rerun_recovery": s["rerun_recovery"],
        "branch_history": s["branch_history"],
        "optional_reviews": s["optional_reviews"],
        "coverage_row": s["coverage_row"],
        "review_lane": s["review_lane"],
        "hold_reason": s["hold_reason"],
        "review_activity": s["review_activity"],
        "receipt": s["receipt"],
        "github_merge_blockers": s["github_merge_blockers"],
        "merge_authority": s["merge_authority"],
        "merge_execution": s["merge_execution"],
        "cache": s["cache"],
    }


def _check_keys(fixture):
    out = json.loads(fixture["expected"]["stdout"])
    extra = sorted(set(out) - ALLOWED_KEYS)
    if extra:
        raise SystemExit(
            f"{fixture['name']}: payload keys outside the Context contract: {extra}. "
            "Add them to the plan's key list first, then re-capture."
        )


if __name__ == "__main__":
    main()
