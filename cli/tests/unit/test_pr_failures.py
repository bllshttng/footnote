"""Failure-detail extraction for `fno do pr status` (x-c124, ruling d-bdb035b6).

A check NAME is not a failure: PR 1069 (pytest assertions) and PR 1059 (ruff
E402, pytest already green) both read `smoke` red fifteen minutes apart. These
tests pin the three facts that separate them - failing step, first error in
that step's block, steps fail-fast never reached - against both incident
shapes plus the real GitHub steps[] payload captured 2026-08-23, and the
loud-degradation contract (an unfetchable log is reported, never dropped).
"""
from __future__ import annotations

import json

import pytest

from fno.pr import _failures
from fno.pr._proc import Result


@pytest.fixture(autouse=True)
def _stub_rust_verdict_reader(monkeypatch):
    """`first_error` is a transport; the verdict logic lives in fno-agents
    (`status-failure-cause`). Default every test to a canned answer so no
    test depends on a built binary; the tests below that pin the transport
    or the cause content override this stub."""
    import fno.rust_binary as rust_binary

    monkeypatch.setattr(
        rust_binary,
        "verb_call",
        lambda verb, payload, **kw: {"cause": "canned cause from the stub", "source": "error_line"},
    )


def _ts(line: str) -> str:
    """A raw Actions-log line: GitHub prefixes every line with a timestamp."""
    return f"2026-08-22T14:38:37.1234567Z\t{line}"


# --- failing_step -----------------------------------------------------------


def test_failing_step_reads_the_current_wording() -> None:
    log = "smoke: step failed, stopping (fail-fast): Pytest (unit + integration)\n"
    assert _failures.failing_step(log) == "Pytest (unit + integration)"


def test_failing_step_reads_the_older_incident_wording() -> None:
    log = "step failed, stopping fail-fast: ruff + mypy (both repo-wide)\n"
    assert _failures.failing_step(log) == "ruff + mypy (both repo-wide)"


def test_failing_step_tolerates_timestamp_prefixes_and_is_none_when_absent() -> None:
    assert _failures.failing_step(_ts("smoke: step failed, stopping (fail-fast): Lint\n")) == "Lint"
    assert _failures.failing_step("all green\n") is None


# --- first_error -------------------------------------------------------------


def test_first_error_is_a_transport_to_the_rust_verdict_reader(monkeypatch) -> None:
    """The parser moved to fno-agents (one owner for the verdict
    logic, at most 30 added Python lines); this module carries the payload
    and hands back the cause."""
    import fno.rust_binary as rust_binary

    seen: dict = {}

    def fake(verb, payload, **kw):
        seen.update({"verb": verb, **payload})
        return {"cause": "test t panicked at tests/x.rs:535: boom", "source": "cargo"}

    monkeypatch.setattr(rust_binary, "verb_call", fake)
    err = _failures.first_error("raw log", "step name", window=[{"conclusion": "failure"}])
    assert err == "test t panicked at tests/x.rs:535: boom"
    assert seen["verb"] == "authorized-merge"
    assert seen["op"] == "status-failure-cause"
    assert seen["log"] == "raw log"
    assert seen["step"] == "step name"
    assert seen["window"] == [{"conclusion": "failure"}]


def test_first_error_degrades_loudly_when_the_owner_is_unreachable(monkeypatch) -> None:
    import fno.rust_binary as rust_binary

    def boom(verb, payload, **kw):
        raise rust_binary.VerbUnavailable("the fno-agents binary was not found")

    monkeypatch.setattr(rust_binary, "verb_call", boom)
    assert _failures.first_error("log", "step") == (
        "failure cause unreadable: the fno-agents binary was not found"
    )


# --- unreached steps ---------------------------------------------------------


def test_unreached_runner_steps_diffs_prologue_against_completions() -> None:
    log = "\n".join(
        [
            "smoke: mode=FULL steps=3/3",
            "smoke: planned: Lint",
            "smoke: planned: Pytest (unit + integration)",
            "smoke: planned: Registry parity",
            "smoke: fail   10s  Lint",
            "smoke: step failed, stopping (fail-fast): Lint",
        ]
    )
    # Fail-fast stopped at Lint: the later two never ran, and their absence
    # must be NAMED, because an omitted step reads as passed.
    assert _failures.unreached_runner_steps(log) == [
        "Pytest (unit + integration)",
        "Registry parity",
    ]
    complete = log.replace(
        "smoke: fail   10s  Lint\nsmoke: step failed, stopping (fail-fast): Lint",
        "smoke: pass   10s  Lint\nsmoke: pass   5s  Pytest (unit + integration)"
        "\nsmoke: pass   5s  Registry parity",
    )
    assert _failures.unreached_runner_steps(complete) == []


def test_unreached_runner_steps_is_none_on_old_logs_without_prologue() -> None:
    log = "smoke: pass   10s  Lint\nsmoke: step failed, stopping (fail-fast): Lint\n"
    assert _failures.unreached_runner_steps(log) is None


def test_unreached_job_steps_keeps_only_later_skipped_steps() -> None:
    """Reverses the old positional premise: a workflow carrying
    `if: ${{ !cancelled() }}` runs its later steps, so a step after the
    failure can read `success` - PR 1823 named 13 guard steps unreached that
    had run and passed. A later step counts as unreached only when its OWN
    conclusion is `skipped`."""
    steps = [
        {"name": "Set up job", "status": "completed", "conclusion": "success"},
        {"name": "Checkout", "status": "completed", "conclusion": "success"},
        {"name": "cargo test --all-targets", "status": "completed", "conclusion": "failure"},
        {"name": "guard: api budget", "status": "completed", "conclusion": "success"},
        {"name": "cargo test --all-targets (fno mux)", "status": "completed", "conclusion": "skipped"},
        {"name": "Schema parity check", "status": "completed", "conclusion": "skipped"},
        {"name": "Post Checkout", "status": "completed", "conclusion": "success"},
        {"name": "Complete job", "status": "completed", "conclusion": "success"},
    ]
    assert _failures.unreached_job_steps(steps) == [
        "cargo test --all-targets (fno mux)",
        "Schema parity check",
    ]


def test_unreached_job_steps_empty_without_a_failed_step() -> None:
    steps = [{"name": "Run", "status": "completed", "conclusion": "skipped"}]
    assert _failures.unreached_job_steps(steps) == []


# --- collect_failures --------------------------------------------------------


def _actions_check(name: str = "smoke-pytest") -> dict:
    return {
        "name": name,
        "status": "completed",
        "conclusion": "FAILURE",
        "startedAt": "2026-08-22T14:00:00Z",
        "detailsUrl": "https://github.com/Owner/Repo/actions/runs/32579190880/job/97045903772",
    }


def _fake_runner(log_text: str, steps: list, *, log_ok: bool = True, calls=None):
    def r(cmd, cwd=None):
        if calls is not None:
            calls.append(" ".join(cmd[:4]))
        url = cmd[-1] if len(cmd) > 1 else ""
        if url.endswith("/logs"):
            if log_ok:
                return Result(0, log_text, "")
            return Result(1, "", "gh api: HTTP 403: forbidden")
        if "/actions/jobs/" in url:
            return Result(0, json.dumps({"steps": steps}), "")
        return Result(1, "", f"unexpected {url}")

    return r


_RED_LOG = "\n".join(
    [
        "smoke: mode=FULL steps=2/2",
        "smoke: planned: Pytest (unit + integration)",
        "smoke: planned: ruff + mypy (both repo-wide)",
        "::group::Pytest (unit + integration)",
        _ts("FAILED t.py::test_a - assert 1 == 2"),
        "::endgroup::",
        _ts("smoke: fail   30s  Pytest (unit + integration)"),
        _ts("smoke: step failed, stopping (fail-fast): Pytest (unit + integration)"),
    ]
) + "\n"


def test_collect_failures_names_step_error_and_unreached(monkeypatch) -> None:
    import fno.rust_binary as rust_binary

    monkeypatch.setattr(
        rust_binary,
        "verb_call",
        lambda verb, payload, **kw: {"cause": "FAILED t.py::test_a - assert 1 == 2"},
    )
    steps = [
        {"name": "Smoke shard: pytest", "status": "completed", "conclusion": "failure"},
        {"name": "Post Checkout", "status": "completed", "conclusion": "success"},
    ]
    entries = _failures.collect_failures(
        [_actions_check()], runner=_fake_runner(_RED_LOG, steps)
    )
    assert len(entries) == 1
    e = entries[0]
    assert e["check"] == "smoke-pytest"
    assert e["step"] == "Pytest (unit + integration)"
    assert e["first_error"] and "FAILED" in e["first_error"]
    assert e["unreached_steps"] == ["ruff + mypy (both repo-wide)"]
    assert e["job_id"] == "97045903772"


def test_collect_failures_reports_a_status_context_without_pretending_a_log() -> None:
    calls: list[str] = []
    entries = _failures.collect_failures(
        [{"name": None, "context": "stacked-base-guard", "state": "FAILURE"}],
        runner=_fake_runner("", [], calls=calls),
    )
    assert entries[0]["check"] == "stacked-base-guard"
    assert "no job log" in entries[0]["detail"]


def test_collect_failures_carries_a_timeout_annotation_as_first_error() -> None:
    message = "The job has exceeded the maximum execution time of 35m0s"
    check = {**_actions_check(), "timeout": message}
    entries = _failures.collect_failures(
        [check], runner=_fake_runner("", [], log_ok=False)
    )
    assert entries[0]["first_error"] == message


def test_collect_failures_names_the_failed_step_without_a_log() -> None:
    """The no-log branch fetched the job steps and named the steps
    fail-fast never reached, but never the step that failed - the cause
    without the consequence's neighbour."""
    steps = [
        {"name": "Set up job", "status": "completed", "conclusion": "success"},
        {"name": "Checkout", "status": "completed", "conclusion": "success"},
        {"name": "pytest", "status": "completed", "conclusion": "failure"},
        {"name": "ruff + mypy", "status": "completed", "conclusion": "skipped"},
        {"name": "Post Checkout", "status": "completed", "conclusion": "success"},
    ]
    entries = _failures.collect_failures(
        [_actions_check()], runner=_fake_runner("", steps, log_ok=False)
    )
    e = entries[0]
    assert e["step"] == "pytest"
    assert e["unreached_steps"] == ["ruff + mypy"]
    assert "log unavailable" in e["detail"]


def test_collect_failures_replays_a_known_job_id_without_reads() -> None:
    """x-c770: a job id is minted per attempt, so a known id is the same
    completed job - its entry replays and no log or job-object read fires."""
    steps = [
        {"name": "Build", "status": "completed", "conclusion": "failure"},
        {"name": "Test", "status": "completed", "conclusion": "skipped"},
    ]
    runner = _fake_runner("plain output\n", steps)
    first = _failures.collect_failures([_actions_check()], runner=runner)
    replayed = _failures.collect_failures(
        [_actions_check()], runner=runner, known={first[0]["job_id"]: first[0]}
    )
    assert replayed == first
    calls: list[str] = []
    _failures.collect_failures(
        [_actions_check()],
        runner=_fake_runner("plain output\n", steps, calls=calls),
        known={first[0]["job_id"]: first[0]},
    )
    assert calls == [], "a known job id must spend no read"
    assert calls == []  # nothing fetched: there is nothing to fetch


def test_collect_failures_degrades_loudly_when_the_log_is_unavailable() -> None:
    entries = _failures.collect_failures(
        [_actions_check()], runner=_fake_runner("", [], log_ok=False)
    )
    assert "log unavailable" in entries[0]["detail"]
    assert "403" in entries[0]["detail"]


def test_collect_failures_caps_detail_and_says_so() -> None:
    failing = [_actions_check(f"check-{i}") for i in range(7)]
    entries = _failures.collect_failures(failing, runner=_fake_runner("", []))
    assert len(entries) == _failures.MAX_DETAILED_FAILURES + 1
    assert "2 more failing" in entries[-1]["check"]


def test_planned_step_lines_round_trip_through_the_parser() -> None:
    """Emitter and parser share one format: `smoke: planned: <name>`, one line
    per step, so a name containing a comma survives (a joined list would not)."""
    from fno.test_cmd import _planned_step_lines

    commad = "Verb-surface ratchet (real count, both binaries)"
    steps = [("Lint", ".", "x"), ("Pytest (unit + integration)", ".", "y"), (commad, ".", "z")]
    lines = _planned_step_lines(steps, [0, 1, 2])
    assert _failures.unreached_runner_steps("\n".join(lines)) == [
        "Lint",
        "Pytest (unit + integration)",
        commad,
    ]


def test_canonical_smoke_red_path_fetches_no_job_object() -> None:
    """The runner log answers alone (step-failed line + planned prologue), so
    the per-check cost is ONE read: the job object is the fallback, and the
    hot polling path never pays for it."""
    calls: list[str] = []
    entries = _failures.collect_failures(
        [_actions_check()], runner=_fake_runner(_RED_LOG, [], calls=calls)
    )
    assert entries[0]["step"] == "Pytest (unit + integration)"
    assert calls and all(c.endswith("/logs") for c in calls), calls


def test_plain_multi_step_job_reads_step_and_unreached_from_steps(monkeypatch) -> None:
    """A log with no runner markers (a plain Actions job) falls back to the
    job object: the failed step's name and the steps after it."""
    import fno.rust_binary as rust_binary

    monkeypatch.setattr(
        rust_binary,
        "verb_call",
        lambda verb, payload, **kw: {"cause": "Traceback (most recent call last)"},
    )
    plain_log = (
        "::group::cargo test --all-targets\n"
        "collected\n"
        "Traceback (most recent call last)\n"
        "::endgroup::\n"
    )
    steps = [
        {"name": "Set up job", "conclusion": "success"},
        {"name": "cargo test --all-targets", "conclusion": "failure"},
        {"name": "Schema parity check", "conclusion": "skipped"},
        {"name": "Complete job", "conclusion": "success"},
    ]
    entries = _failures.collect_failures(
        [_actions_check("cargo")], runner=_fake_runner(plain_log, steps)
    )
    e = entries[0]
    assert e["step"] == "cargo test --all-targets"
    assert e["first_error"] and "Traceback" in e["first_error"]
    assert e["unreached_steps"] == ["Schema parity check"]
