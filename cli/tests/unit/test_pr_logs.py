"""`fno pr logs` - the agent-facing CI reader (x-32af US2).

`gh` is injected rather than reached, so the failure paths that matter most
(unauthenticated, expired retention, a job that is not an Actions job) are
exercised deterministically instead of only when a real PR happens to be red.
"""
import json

import pytest

from fno.pr import _logs
from fno.pr._proc import Result


def _check(name, conclusion="SUCCESS", status="COMPLETED", job="11", url=None):
    return {
        "__typename": "CheckRun",
        "name": name,
        "status": status,
        "conclusion": conclusion,
        "detailsUrl": url
        if url is not None
        else f"https://github.com/o/r/actions/runs/9/job/{job}",
        "startedAt": "2026-07-24T00:00:00Z",
        "completedAt": "2026-07-24T00:01:00Z",
    }


class _Gh:
    """Stand-in for `fno.pr._proc.run`, recording what was asked of gh."""

    def __init__(self, *, rollup=None, view=None, log="", view_rc=0, log_rc=0, err=""):
        self._view = view if view is not None else json.dumps(
            {"statusCheckRollup": list(rollup or [])}
        )
        self._log, self._view_rc, self._log_rc, self._err = log, view_rc, log_rc, err
        self.calls = []

    def __call__(self, cmd, **kw):
        self.calls.append(list(cmd))
        if cmd[:3] == ["gh", "pr", "view"]:
            return Result(self._view_rc, self._view if self._view_rc == 0 else "", self._err)
        return Result(self._log_rc, self._log if self._log_rc == 0 else "", self._err)

    @property
    def fetched_a_log(self):
        return any(c[:2] == ["gh", "api"] for c in self.calls)


@pytest.fixture
def gh(monkeypatch):
    def _install(**kw):
        fake = _Gh(**kw)
        monkeypatch.setattr(_logs, "run", fake)
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
    assert "authentication" in cap.err
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
    ctx = {"__typename": "StatusContext", "context": "ext/check", "state": "FAILURE",
           "targetUrl": "https://ci.example.com/build/7", "createdAt": "2026-07-24T00:00:00Z"}
    fake = gh(rollup=[ctx])

    rc = _logs.run_logs("1", root=tmp_path)
    out = capsys.readouterr().out

    assert rc == 1
    assert "https://ci.example.com/build/7" in out
    assert not fake.fetched_a_log
    assert not _spooled(tmp_path).exists()


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
    stale["startedAt"] = "2026-07-24T00:00:00Z"
    fresh = _check("cli-ci", "SUCCESS", job="12")
    fresh["startedAt"] = "2026-07-24T05:00:00Z"
    fake = gh(rollup=[stale, fresh])

    assert _logs.run_logs("1", root=tmp_path) == 0
    assert not fake.fetched_a_log
