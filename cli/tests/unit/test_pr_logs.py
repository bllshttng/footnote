"""`fno do pr logs` - the agent-facing CI reader (x-32af US2).

`gh` is injected rather than reached, so the failure paths that matter most
(unauthenticated, expired retention, a job that is not an Actions job) are
exercised deterministically instead of only when a real PR happens to be red.
"""
import json

import pytest

from fno.pr import _logs, _rest
from fno.pr._proc import Result


def _check(name, conclusion="SUCCESS", status="COMPLETED", job="11", url=None):
    # REST-native row shape (lowercase enums, details_url): the rollup source
    # is `fetch_pr_rest`, never `gh pr view`.
    return {
        "name": name,
        "status": status.lower(),
        "conclusion": conclusion.lower() if conclusion else "",
        "started_at": "2026-07-24T00:00:00Z",
        "details_url": url
        if url is not None
        else f"https://github.com/o/r/actions/runs/9/job/{job}",
    }


class _Gh:
    """Stand-in for `fno.pr._proc.run`, recording what was asked of gh/git."""

    def __init__(self, *, rollup=None, statuses=None, log="", view_rc=0, log_rc=0, err=""):
        self._rollup = list(rollup or [])
        self._statuses = list(statuses or [])
        self._log, self._view_rc, self._log_rc, self._err = log, view_rc, log_rc, err
        self.calls = []

    def __call__(self, cmd, **kw):
        self.calls.append(list(cmd))
        url = cmd[-1] if len(cmd) > 2 else ""
        if cmd[0] == "git":
            if "get-url" in cmd:
                return Result(0, "https://github.com/o/r.git", "")
            return Result(0, "feature/x\n", "")
        if self._view_rc != 0:
            # The status read itself fails (auth, rate limit, transport).
            return Result(self._view_rc, "", self._err)
        if "/actions/jobs/" in url:
            return Result(self._log_rc, self._log if self._log_rc == 0 else "", self._err)
        if "check-runs" in url:
            return Result(
                0,
                json.dumps({"total_count": len(self._rollup), "check_runs": self._rollup}),
                "",
            )
        if url.endswith("/status"):
            return Result(0, json.dumps({"statuses": self._statuses}), "")
        if "/pulls?" in url:
            return Result(0, json.dumps([{"number": 42, "state": "open"}]), "")
        if "/pulls/" in url:
            return Result(
                0,
                json.dumps(
                    {
                        "html_url": "https://github.com/o/r/pull/1",
                        "state": "open",
                        "merged": False,
                        "head": {"sha": "abc123", "ref": "feature/x"},
                        "base": {"ref": "main"},
                        "mergeable": True,
                    }
                ),
                "",
            )
        return Result(1, "", f"unexpected: {' '.join(cmd)}")

    @property
    def fetched_a_log(self):
        return any(
            c[:2] == ["gh", "api"] and "/actions/jobs/" in c[-1] for c in self.calls
        )


@pytest.fixture
def gh(monkeypatch):
    def _install(**kw):
        fake = _Gh(**kw)
        monkeypatch.setattr(_logs, "run", fake)
        # The real REST reader runs against the fake runner: its `runner`
        # default binds at import, so the injection has to happen through a
        # wrapper, not by patching `_rest.run` (which the default never reads).
        real_fetch = _rest.fetch_pr_rest
        real_resolve = _rest.resolve_current_pr_number_rest
        monkeypatch.setattr(
            _rest,
            "fetch_pr_rest",
            lambda pr, cwd=None, runner=None: real_fetch(pr, cwd=cwd, runner=fake),
        )
        monkeypatch.setattr(
            _rest,
            "resolve_current_pr_number_rest",
            lambda **kw: real_resolve(runner=fake, **kw),
        )
        return fake

    return _install


def _spooled(tmp_path):
    return tmp_path / ".fno" / "last-ci.log"


def test_failing_check_tails_and_spools(gh, tmp_path, capsys):
    """AC1/AC5: a huge log becomes a small stdout plus a complete file."""
    big = "".join(f"line {i} of a very chatty CI job\n" for i in range(20_000))
    assert len(big) > 400_000
    fake = gh(rollup=[_check("cli-ci", "FAILURE")], log=big)

    rc = _logs.run_logs("1", root=tmp_path)
    out = capsys.readouterr().out

    assert rc == 1
    assert len(out.encode()) < 4096, f"stdout was {len(out.encode())} bytes"
    assert "cli-ci" in out
    assert str(_spooled(tmp_path)) in out
    # AC5: the spool is the whole log, and its tail is what was printed.
    assert _spooled(tmp_path).read_text() == big
    tail = "".join(big.splitlines(keepends=True)[-40:])
    assert tail in out
    assert fake.fetched_a_log


def test_all_green_fetches_nothing(gh, tmp_path, capsys):
    """AC2: the green path makes no log call and leaves no spool behind."""
    fake = gh(rollup=[_check("a"), _check("b", job="12")])

    rc = _logs.run_logs("1", root=tmp_path)

    assert rc == 0
    assert "all 2 checks green" in capsys.readouterr().out
    assert not fake.fetched_a_log
    assert not _spooled(tmp_path).exists()


def test_unauthenticated_never_prints_green(gh, tmp_path, capsys):
    """AC3: the wrong-but-passing shape - green because it could not see."""
    gh(view_rc=1, err="HTTP 401: Bad credentials")

    rc = _logs.run_logs("1", root=tmp_path)
    cap = capsys.readouterr()

    assert rc == 4
    assert "authentication" in cap.err.lower()
    assert "green" not in cap.out


def test_rate_limit_is_named(gh, tmp_path, capsys):
    gh(view_rc=1, err="HTTP 403: API rate limit exceeded")
    assert _logs.run_logs("1", root=tmp_path) == 4
    assert "rate limit" in capsys.readouterr().err


def test_pending_is_neither_pass_nor_fail(gh, tmp_path, capsys):
    """AC4: a running check has an empty conclusion and must not read green."""
    gh(rollup=[_check("done"), _check("running", "", "IN_PROGRESS", job="12")])

    rc = _logs.run_logs("1", root=tmp_path)
    out = capsys.readouterr().out

    assert rc == 2
    assert "running" in out
    assert "green" not in out


def test_no_checks_is_its_own_code(gh, tmp_path, capsys):
    gh(rollup=[])
    assert _logs.run_logs("1", root=tmp_path) == 3
    assert "no checks" in capsys.readouterr().out


def test_short_log_is_printed_whole(gh, tmp_path, capsys):
    gh(rollup=[_check("cli-ci", "FAILURE")], log="only\ntwo\n")
    rc = _logs.run_logs("1", root=tmp_path)
    out = capsys.readouterr().out
    assert rc == 1
    assert "only\ntwo\n" in out
    assert "last 2 lines" in out


def test_every_failure_is_named_and_others_reachable(gh, tmp_path, capsys):
    rollup = [_check("cli-ci", "FAILURE"), _check("rust-ci", "FAILURE", job="12")]
    fake = gh(rollup=rollup, log="rust output\n")

    rc = _logs.run_logs("1", job="rust", root=tmp_path)
    out = capsys.readouterr().out

    assert rc == 1
    assert "cli-ci" in out and "rust-ci" in out
    assert "rust output" in out
    assert any("12" in c[-1] for c in fake.calls if c[:2] == ["gh", "api"])


def test_unknown_job_filter_reports_the_real_names(gh, tmp_path, capsys):
    gh(rollup=[_check("cli-ci", "FAILURE")], log="x\n")
    rc = _logs.run_logs("1", job="nope", root=tmp_path)
    assert rc == 1
    assert "cli-ci" in capsys.readouterr().err


def test_non_actions_check_reports_its_url(gh, tmp_path, capsys):
    """A StatusContext has no job log; say so rather than spool an empty file."""
    ctx = {"context": "ext/check", "state": "failure",
           "target_url": "https://ci.example.com/build/7", "created_at": "2026-07-24T00:00:00Z"}
    fake = gh(statuses=[ctx])

    rc = _logs.run_logs("1", root=tmp_path)
    out = capsys.readouterr().out

    assert rc == 1
    assert "https://ci.example.com/build/7" in out
    assert not fake.fetched_a_log
    assert not _spooled(tmp_path).exists()


def test_rollup_read_is_rest_never_graphql(gh, tmp_path, capsys):
    """x-4eac: the rollup read spends the core REST budget, never the shared
    per-USER GraphQL quota `gh pr view` bills against."""
    fake = gh(rollup=[_check("cli-ci", "FAILURE")], log="boom\n")
    assert _logs.run_logs("1", root=tmp_path) == 1
    capsys.readouterr()
    assert not any(c[:3] == ["gh", "pr", "view"] for c in fake.calls)
    assert any("check-runs" in c[-1] for c in fake.calls)


def test_expired_log_says_retention(gh, tmp_path, capsys):
    gh(rollup=[_check("cli-ci", "FAILURE")], log_rc=1, err="HTTP 410: Gone")
    rc = _logs.run_logs("1", root=tmp_path)
    assert rc == 1
    assert "expired" in capsys.readouterr().err


def test_full_prints_everything(gh, tmp_path, capsys):
    body = "".join(f"{i}\n" for i in range(100))
    gh(rollup=[_check("cli-ci", "FAILURE")], log=body)
    rc = _logs.run_logs("1", full=True, root=tmp_path)
    out = capsys.readouterr().out
    assert rc == 1
    assert body in out


def test_spool_write_failure_is_reported(gh, tmp_path, capsys, monkeypatch):
    """A fetched log that cannot be written is an error, never a silent drop."""
    gh(rollup=[_check("cli-ci", "FAILURE")], log="boom\n")
    monkeypatch.setattr(
        _logs.Path, "write_text", lambda *a, **k: (_ for _ in ()).throw(OSError("full"))
    )
    rc = _logs.run_logs("1", root=tmp_path)
    assert rc == 1
    assert "could not write" in capsys.readouterr().err


def test_superseded_cancelled_run_is_not_a_failure(gh, tmp_path, capsys):
    """Reuses _status's dedupe: a stale CANCELLED must not read as red."""
    stale = _check("cli-ci", "CANCELLED")
    stale["started_at"] = "2026-07-24T00:00:00Z"
    fresh = _check("cli-ci", "SUCCESS", job="12")
    fresh["started_at"] = "2026-07-24T05:00:00Z"
    fake = gh(rollup=[stale, fresh])

    assert _logs.run_logs("1", root=tmp_path) == 0
    assert not fake.fetched_a_log
