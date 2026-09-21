"""Rerun recovery: the fact a latest-rollup verdict cannot show.

`verdict_for` reads only the latest attempt per check name, so a CI failure
recovered by a re-run reads green exactly like never-failed. That silent path
merged a shard-ordering flake and reded main with it. These pin the recovery
probe's pure core, its fail-open wrapper, the status payload fields, and the
human note.
"""

import json

from fno.pr import _status


def _run(run_id, conclusion, attempt):
    return {"id": run_id, "conclusion": conclusion, "run_attempt": attempt}


def _att(n, conclusion):
    return {"run_attempt": n, "conclusion": conclusion}


def _core(rows, attempts, jobs):
    return _status._recovery_from_run_rows(
        rows,
        lambda rid: attempts.get(rid, []),
        lambda rid, n: jobs.get((rid, n), []),
    )


# ---- pure core ----


def test_failed_earlier_attempt_latest_pass_is_recovered():
    rows = [_run("1", "success", 2)]
    attempts = {"1": [_att(1, "failure"), _att(2, "success")]}
    jobs = {("1", 1): ["smoke-pytest (7)"]}
    assert _core(rows, attempts, jobs) == {
        "recovered": True,
        "failed": ["smoke-pytest (7)"],
    }


def test_first_attempt_pass_is_never_recovered():
    assert _core([_run("2", "success", 1)], {"2": []}, {}) == {
        "recovered": False,
        "failed": [],
    }


def test_rerun_of_a_green_run_is_not_recovered():
    # "Re-run all jobs" on an already-green run: attempts grew, nothing failed.
    rows = [_run("3", "success", 2)]
    attempts = {"3": [_att(1, "success"), _att(2, "success")]}
    assert _core(rows, attempts, {}) == {"recovered": False, "failed": []}


def test_latest_still_red_is_not_recovered():
    rows = [_run("4", "failure", 2)]
    attempts = {"4": [_att(1, "failure"), _att(2, "failure")]}
    assert _core(rows, attempts, {}) == {"recovered": False, "failed": []}


def test_cancelled_earlier_attempt_is_not_a_failed_attempt():
    rows = [_run("5", "success", 2)]
    attempts = {"5": [_att(1, "cancelled"), _att(2, "success")]}
    assert _core(rows, attempts, {}) == {"recovered": False, "failed": []}


def test_failed_attempt_with_no_job_names_still_recovers():
    # The attempt row proves recovery on its own; job names are diagnostics.
    rows = [_run("6", "success", 2)]
    attempts = {"6": [_att(1, "startup_failure"), _att(2, "success")]}
    assert _core(rows, attempts, {}) == {"recovered": True, "failed": []}


# ---- wrapper fail-open ----


class _FailedRes:
    ok = False
    stdout = ""
    stderr = "boom"


def _stub_world(monkeypatch, res):
    from fno.pr import _proc as proc_mod
    from fno.pr import _rest as rest_mod

    monkeypatch.setattr(
        rest_mod, "_slug_or_reason", lambda cwd, runner=None, repo=None: ("o/r", "")
    )
    monkeypatch.setattr(
        rest_mod,
        "fetch_pr_info_rest",
        lambda pr, cwd=None, runner=None, repo=None: ({"head_sha": "abc123"}, ""),
    )
    monkeypatch.setattr(proc_mod, "run", lambda *a, **k: res)


def test_wrapper_fails_open_when_the_runs_read_errors(monkeypatch):
    _stub_world(monkeypatch, _FailedRes())
    assert _status.rerun_recovery("42") == {"recovered": False, "failed": []}


def test_wrapper_fails_open_on_malformed_json(monkeypatch):
    class _BadJsonRes:
        ok = True
        stdout = "not json"
        stderr = ""

    _stub_world(monkeypatch, _BadJsonRes())
    assert _status.rerun_recovery("42") == {"recovered": False, "failed": []}


def test_wrapper_fails_open_when_gh_is_missing(monkeypatch):
    from fno.pr import _proc as proc_mod
    from fno.pr import _rest as rest_mod

    monkeypatch.setattr(
        rest_mod, "_slug_or_reason", lambda cwd, runner=None, repo=None: ("o/r", "")
    )
    monkeypatch.setattr(
        rest_mod,
        "fetch_pr_info_rest",
        lambda pr, cwd=None, runner=None, repo=None: ({"head_sha": "abc123"}, ""),
    )

    def _boom(*a, **k):
        raise FileNotFoundError("gh")

    monkeypatch.setattr(proc_mod, "run", _boom)
    assert _status.rerun_recovery("42") == {"recovered": False, "failed": []}


# ---- status payload ----


def _run_status(monkeypatch, capsys, rollup, rerun=None, decision=None):
    """run_status with gh stubbed out; returns (exit code, parsed JSON, stderr).

    The rerun fact rides the decision receipt now: the walk probes recovery
    itself, so the stub shapes both the blockers and the probed fact.
    """
    from fno.pr import _merge as merge_mod

    monkeypatch.setattr(
        merge_mod, "_code_review_attestation_required", lambda repo, pr_number=0: False
    )
    monkeypatch.setattr(
        _status,
        "_fetch",
        lambda pr, cwd: ({"state": "OPEN", "statusCheckRollup": rollup}, ""),
    )
    monkeypatch.setattr(
        _status,
        "read_optional_review_state",
        lambda pr, cwd: {"optional_reviews": [], "optional_reviews_unresolved": 0},
    )
    monkeypatch.setattr(
        _status,
        "read_review_coverage",
        lambda pr, cwd, **kw: {"coverage": "unknown", "reviewed_count": None},
    )
    monkeypatch.setattr(_status, "_review_lane", lambda pr, cwd: True)
    if rerun is not None or decision is not None:
        receipt = {
            "outcome": "held" if decision else "authorized",
            "blockers": [
                {"code": code, "class": "held", "detail": code}
                for code in (decision or [])
            ],
        }
        if rerun is not None:
            receipt["rerun_recovered"] = rerun["recovered"]
            receipt["recovered_failures"] = list(rerun.get("failed") or [])
        monkeypatch.setattr(
            _status, "_merge_decision", lambda pr, repo, facts: receipt
        )
    code = _status.run_status("42")
    cap = capsys.readouterr()
    return code, json.loads(cap.out), cap.err


_GREEN_ROLLUP = [{"name": "smoke-pytest (7)", "status": "COMPLETED", "conclusion": "SUCCESS"}]


def test_recovered_green_payload_names_the_failed_checks(monkeypatch, capsys):
    code, out, err = _run_status(
        monkeypatch,
        capsys,
        _GREEN_ROLLUP,
        rerun={"recovered": True, "failed": ["smoke-pytest (7)"]},
        decision=["rerun_recovered_green"],
    )
    assert code == 0
    assert out["verdict"] == "green"
    assert out["rerun_recovered"] is True
    assert out["recovered_failures"] == ["smoke-pytest (7)"]
    assert "green on re-run" in err
    assert "smoke-pytest (7)" in err
    # The ready conjunct agrees with the merge gate: a held merge is not ready.
    assert out["ready"] is False
    assert "rerun_recovered_green" in out["ready_blockers"]


def test_clean_green_payload_carries_the_probed_false(monkeypatch, capsys):
    code, out, _err = _run_status(
        monkeypatch,
        capsys,
        _GREEN_ROLLUP,
        rerun={"recovered": False, "failed": []},
        decision=[],
    )
    assert code == 0
    assert out["rerun_recovered"] is False
    assert out["recovered_failures"] == []


def test_non_green_read_omits_the_keys(monkeypatch, capsys):
    rollup = [
        {"name": "ci", "status": "COMPLETED", "conclusion": "FAILURE"},
    ]
    code, out, _err = _run_status(monkeypatch, capsys, rollup)
    assert code == 1
    assert "rerun_recovered" not in out
    assert "recovered_failures" not in out
