"""Tests for the surviving REST readers (`fno.pr._rest`).

The load-bearing guard: the settledness reader issues NO GraphQL call, so a
watching fleet spends the idle core budget instead of the shared per-USER
GraphQL quota. The PR rollup reader itself is the Rust owner now
(crates/fno-agents/src/pr_status.rs); what stays here pins the metadata,
files, PR-listing and reason-classification contracts.
"""
from __future__ import annotations

import json

import pytest

from fno.pr import _quota, _rest
from fno.pr._proc import Result

_PULLS = {
    "html_url": "https://github.com/Owner/Repo/pull/42",
    "state": "open",
    "merged": False,
    "head": {"sha": "abc123def", "ref": "feature/test"},
    "base": {"ref": "main"},
}
_GH_URL = "git@github.com:Owner/Repo.git"


def _runner(
    *, pulls=_PULLS, check_runs=(), statuses=None, workflow_runs=(), fail=None, calls=None
):
    """Dispatch by URL shape: git remote -> slug, gh api -> REST payloads."""

    def r(cmd, cwd=None):
        if calls is not None:
            calls.append(list(cmd))
        url = cmd[-1] if len(cmd) > 1 else ""
        if fail is not None and fail(cmd):
            return Result(1, "", fail(cmd))
        if cmd[:2] == ["git", "remote"]:
            return Result(0, _GH_URL, "")
        if "/pulls/" in url:
            return Result(0, json.dumps(pulls), "")
        if "check-runs" in url:
            return Result(0, json.dumps({"check_runs": list(check_runs)}), "")
        if "actions/runs?" in url:
            return Result(
                0,
                json.dumps({"total_count": len(list(workflow_runs)), "workflow_runs": list(workflow_runs)}),
                "",
            )
        if url.endswith("/status"):
            return Result(0, json.dumps({"statuses": list(statuses or [])}), "")
        return Result(1, "", "unexpected: " + " ".join(cmd))

    return r


def _cr(name, status, conclusion="", started="2026-08-14T10:00:00Z"):
    return {"name": name, "status": status, "conclusion": conclusion, "started_at": started}


def test_pr_info_uses_one_rest_request_and_returns_positive_metadata():
    calls: list[list[str]] = []
    pulls = {
        "html_url": "https://github.com/Owner/Repo/pull/42",
        "state": "open",
        "merged": False,
        "mergeable": True,
        "head": {"sha": "abc123def", "ref": "feature/rest-info"},
        "base": {"ref": "main"},
        "user": {"login": "alice"},
    }
    info, reason = _rest.fetch_pr_info_rest(
        "42", repo="Owner/Repo", runner=_runner(pulls=pulls, calls=calls)
    )
    assert reason == ""
    assert info == {
        "pr": 42,
        "url": "https://github.com/Owner/Repo/pull/42",
        "body": "",
        "state": "OPEN",
        "head_sha": "abc123def",
        "head_ref": "feature/rest-info",
        "base_ref": "main",
        "mergeable": "MERGEABLE",
        # The REST spelling of mergeStateStatus rides the same payload; the
        # fixture carries none, so the mapped value stays None.
        "merge_state_status": None,
        "merged_at": None,
        "merge_sha": None,
        "author": "alice",
        # Whether GitHub's auto-merge queue owns the PR rides THIS payload, so
        # the armed flag and the head it was read against come from one fetch.
        "auto_merge": None,
    }
    assert calls == [["gh", "api", "repos/Owner/Repo/pulls/42"]]


def test_pr_info_carries_the_body_verbatim_for_the_binding_gate():
    pulls = {
        "html_url": "https://github.com/Owner/Repo/pull/42",
        "state": "open",
        "merged": False,
        "head": {"sha": "abc123def", "ref": "feature/x-0001"},
        "base": {"ref": "main"},
        "body": "Summary.\n\nBacklog-Closure: x-0001\n",
    }
    info, reason = _rest.fetch_pr_info_rest(
        "42", repo="Owner/Repo", runner=_runner(pulls=pulls)
    )
    assert reason == ""
    assert info is not None
    assert info["body"] == "Summary.\n\nBacklog-Closure: x-0001\n"


def test_pr_info_reads_a_null_body_as_empty():
    pulls = {
        "html_url": "https://github.com/Owner/Repo/pull/42",
        "state": "open",
        "merged": False,
        "head": {"sha": "abc123def", "ref": "feature/x-0001"},
        "base": {"ref": "main"},
        "body": None,
    }
    info, reason = _rest.fetch_pr_info_rest(
        "42", repo="Owner/Repo", runner=_runner(pulls=pulls)
    )
    assert reason == ""
    assert info is not None
    assert info["body"] == ""


def test_pr_info_carries_the_auto_merge_object_when_the_queue_owns_the_pr():
    pulls = {
        "html_url": "https://github.com/Owner/Repo/pull/42",
        "state": "open",
        "merged": False,
        "mergeable": True,
        "head": {"sha": "abc123def", "ref": "feature/rest-info"},
        "base": {"ref": "main"},
        "user": {"login": "alice"},
        "auto_merge": {"merge_method": "merge", "enabled_by": {"login": "alice"}},
    }
    info, reason = _rest.fetch_pr_info_rest(
        "42", repo="Owner/Repo", runner=_runner(pulls=pulls)
    )
    assert reason == ""
    assert info is not None
    assert info["auto_merge"] == {
        "merge_method": "merge",
        "enabled_by": {"login": "alice"},
    }


def test_pr_info_refuses_a_malformed_auto_merge_object():
    pulls = {
        "html_url": "https://github.com/Owner/Repo/pull/42",
        "state": "open",
        "merged": False,
        "mergeable": True,
        "head": {"sha": "abc123def", "ref": "feature/rest-info"},
        "base": {"ref": "main"},
        "auto_merge": "yes",
    }
    info, reason = _rest.fetch_pr_info_rest(
        "42", repo="Owner/Repo", runner=_runner(pulls=pulls)
    )
    assert info is None
    assert "auto_merge" in reason


def test_pr_info_preserves_unknown_mergeability():
    pulls = {
        "html_url": "https://github.com/Owner/Repo/pull/42",
        "state": "open",
        "merged": False,
        "mergeable": None,
        "head": {"sha": "abc123def", "ref": "feature/rest-info"},
        "base": {"ref": "main"},
    }
    info, reason = _rest.fetch_pr_info_rest(
        "42", repo="Owner/Repo", runner=_runner(pulls=pulls)
    )
    assert reason == ""
    assert info["mergeable"] == "UNKNOWN"


def test_pr_info_carries_merge_commit_sha():
    pulls = dict(_PULLS, state="closed", merged=True, merged_at="2026-08-25T12:00:00Z", merge_commit_sha="merge123")
    info, reason = _rest.fetch_pr_info_rest(
        "42", repo="Owner/Repo", runner=_runner(pulls=pulls)
    )
    assert reason == ""
    assert info is not None
    assert info["merge_sha"] == "merge123"


def test_pr_file_paths_rest_paginates_until_short_page():
    calls: list[list[str]] = []

    def runner(cmd, cwd=None):
        calls.append(list(cmd))
        if cmd[-1].endswith("page=1"):
            return Result(0, json.dumps([{"filename": f"f{i}.py"} for i in range(100)]), "")
        if cmd[-1].endswith("page=2"):
            return Result(0, json.dumps([{"filename": "tail.py"}]), "")
        raise AssertionError(f"unexpected command: {cmd}")

    paths, reason = _rest.fetch_pr_file_paths_rest("42", repo="Owner/Repo", runner=runner)

    assert reason == ""
    assert paths == [f"f{i}.py" for i in range(100)] + ["tail.py"]
    assert [call[-1].endswith("page=1") for call in calls] == [True, False]


def test_pr_file_paths_rest_rejects_malformed_page_rows():
    def runner(cmd, cwd=None):
        return Result(0, json.dumps([{"filename": "ok.py"}, {"filename": 7}]), "")

    paths, reason = _rest.fetch_pr_file_paths_rest("42", repo="Owner/Repo", runner=runner)

    assert paths is None
    assert "malformed" in reason


def test_pr_file_paths_rest_rejects_a_failed_second_page():
    def runner(cmd, cwd=None):
        if cmd[-1].endswith("page=1"):
            return Result(0, json.dumps([{"filename": f"f{i}.py"} for i in range(100)]), "")
        return Result(1, "", "i/o timeout")

    paths, reason = _rest.fetch_pr_file_paths_rest("42", repo="Owner/Repo", runner=runner)

    assert paths is None
    assert "timeout" in reason


def test_pr_file_paths_rest_fails_closed_at_github_cap():
    def runner(cmd, cwd=None):
        return Result(0, json.dumps([{"filename": "full.py"} for _ in range(100)]), "")

    paths, reason = _rest.fetch_pr_file_paths_rest("42", repo="Owner/Repo", runner=runner)

    assert paths is None
    assert "3,000-file cap" in reason


def test_pr_info_rejects_malformed_head_shape():
    info, reason = _rest.fetch_pr_info_rest(
        "42",
        repo="Owner/Repo",
        runner=_runner(pulls={"state": "open", "head": [], "base": {"ref": "main"}}),
    )
    assert info is None
    assert "malformed head/base" in reason


def test_pr_info_allows_missing_html_url_without_losing_metadata():
    pulls = dict(_PULLS)
    pulls.pop("html_url")
    info, reason = _rest.fetch_pr_info_rest(
        "42", repo="Owner/Repo", runner=_runner(pulls=pulls)
    )
    assert reason == ""
    assert info is not None
    assert info["url"] is None
    assert info["head_sha"] == "abc123def"


def test_current_pr_number_uses_rest_not_gh_pr_view():
    calls: list[list[str]] = []

    def runner(cmd, cwd=None):
        calls.append(list(cmd))
        if cmd[:3] == ["git", "branch", "--show-current"]:
            return Result(0, "feature/rest-info\n", "")
        if cmd[:2] == ["gh", "api"]:
            return Result(0, '[{"number":930}]', "")
        return Result(1, "", "unexpected")

    number, reason = _rest.resolve_current_pr_number_rest(
        repo="Owner/Repo", runner=runner
    )
    assert (number, reason) == (930, "")
    assert calls == [
        ["git", "branch", "--show-current"],
        [
            "gh", "api",
            "repos/Owner/Repo/pulls?state=all&head=Owner:feature/rest-info&per_page=2",
        ],
    ]


_VERBATIM_403 = (
    "gh: API rate limit exceeded for user ID 4994564. If you reach out to "
    "GitHub Support for help, please include the request ID "
    "FAEB:283161:6EF36:99B72:6A8B97DD ... Terms of Service (...) (HTTP 403)"
)


def _rate_limit_runner(core_remaining=None):
    """Answer `gh api rate_limit` with the named core reading.

    None means the instrument itself cannot answer (the endpoint is exempt,
    but the read can still die), which the classifier must read as unknown.
    """

    def r(cmd, cwd=None, timeout=None):
        assert cmd[:3] == ["gh", "api", "rate_limit"], f"unexpected: {cmd}"
        if core_remaining is None:
            return Result(1, "", "instrument unreadable")
        return Result(
            0,
            json.dumps(
                {
                    "resources": {
                        "core": {"remaining": core_remaining, "limit": 5000, "reset": 4102444800},
                        "graphql": {"remaining": 4446, "limit": 5000, "reset": 4102444800},
                    }
                }
            ),
            "",
        )

    return r


def test_the_reason_diagnostic_read_is_bounded_below_any_callers_budget():
    """The bucket read only DECORATES an error the caller already has, so it
    may never cost more than the read it explains. It passed its own 30s
    default, which wins over the runner's, so a read_pr_state declaring 3s
    spent 3s failing and up to 30s more explaining why."""
    seen = {}

    def r(cmd, cwd=None, timeout=None):
        seen["timeout"] = timeout
        return Result(1, "", "instrument unreadable")

    _rest._rest_reason(Result(1, "", "gh: API rate limit exceeded (HTTP 403)"), runner=r)

    assert seen["timeout"] == _rest._REASON_DIAGNOSTIC_TIMEOUT_S
    assert seen["timeout"] < 30.0


def test_verbatim_403_with_healthy_core_reads_secondary_and_carries_the_verdict():
    """The p0 fixture: the measured 403 body says only `API rate limit
    exceeded` (no `secondary` anywhere) while the exempt bucket answers
    4980/5000 - that IS the secondary limit. The reason must carry the verdict
    as data for the cache, and the prose must still say back off."""
    assert "secondary" not in _VERBATIM_403.lower()
    reason = _rest._rest_reason(
        Result(1, "", _VERBATIM_403), runner=_rate_limit_runner(core_remaining=4980)
    )
    assert reason.rate_limit_class == "secondary"
    assert "SECONDARY" in reason
    assert "4980" in reason
    assert "back off" in reason.lower()


def test_drained_core_bucket_classifies_core_and_names_the_reading():
    """Core quota: the same `rate limit` wording but the live bucket reads 0.
    The bucket, not the wording, picks the branch."""
    res = Result(1, "", "gh: API rate limit exceeded (HTTP 403)")
    reason = _rest._rest_reason(res, runner=_rate_limit_runner(core_remaining=0))
    assert reason.rate_limit_class == "core"
    assert "CORE" in reason
    assert "resources.core" in reason


def test_low_but_positive_core_is_not_proof_of_the_core_quota():
    """A secondary refusal lands with core wherever it stood; a low-but-
    positive reading is not evidence the core quota refused. Only 0 is CORE
    - mislabeling secondary as CORE sends the fleet to wait for a reset
    instead of backing off, the exact harm this classifier exists to
    prevent."""
    res = Result(1, "", "gh: API rate limit exceeded (HTTP 403)")
    reason = _rest._rest_reason(res, runner=_rate_limit_runner(core_remaining=5))
    assert reason.rate_limit_class == "secondary"
    assert "back off" in reason.lower()


def test_the_phrase_does_not_classify_the_bucket_does():
    """Even stderr that DOES say `secondary rate limit` classifies by the live
    bucket: wording is GitHub's to change, so it is never the discriminator."""
    res = Result(1, "", "HTTP 403: You have exceeded a secondary rate limit")
    core = _rest._rest_reason(res, runner=_rate_limit_runner(core_remaining=0))
    healthy = _rest._rest_reason(res, runner=_rate_limit_runner(core_remaining=4980))
    assert core.rate_limit_class == "core"
    assert healthy.rate_limit_class == "secondary"


def test_unreadable_bucket_still_fails_toward_back_off():
    """No instrument (no runner passed, or the rate_limit read died): reading
    unknown as CORE sends the fleet to wait for a reset that never comes, so
    the unknown case classifies secondary and tells the caller to back off."""
    no_instrument = _rest._rest_reason(
        Result(1, "", _VERBATIM_403), runner=_rate_limit_runner(core_remaining=None)
    )
    no_runner = _rest._rest_reason(Result(1, "", _VERBATIM_403))
    for reason in (no_instrument, no_runner):
        assert reason.rate_limit_class == "secondary"
        assert "back off" in reason.lower()


def test_wrapper_warning_on_line_1_is_not_the_quoted_cause():
    """The gh-proxy shim's own startup lines ride the same captured stderr as
    gh's error. The quoted evidence must be the MATCHED line, so fno's config
    deprecation warning is never blamed for a rate-limit refusal (it was,
    measured on `fno do pr info`)."""
    stderr = (
        "fno config: [agents] max_lanes is renamed provider_limits; the legacy"
        " spelling still parses (x-3f84)\n" + _VERBATIM_403
    )
    reason = _rest._rest_reason(
        Result(1, "", stderr), runner=_rate_limit_runner(core_remaining=4980)
    )
    assert "API rate limit exceeded for user ID 4994564" in reason
    assert "max_lanes" not in reason
    assert reason.rate_limit_class == "secondary"


# ---- the secondary arm records the refusal in the fleet budget ledger ----


@pytest.fixture(autouse=True)
def quiet_budget(monkeypatch):
    """Keep every classification test off the real fleet ledger: a test that
    classifies a verbatim 403 through the REAL chain would open a live 60s
    backoff on the operator's machine-wide budget. The three door tests below
    restore the real record_refusal from the import-time capture."""
    monkeypatch.setattr("fno.pr._quota.record_refusal", lambda text: None)
    monkeypatch.setattr("fno.pr._quota.admit", lambda argv: None)


_REAL_RECORD_REFUSAL = _quota.record_refusal


def test_a_secondary_classification_records_the_refusal_once(monkeypatch):
    monkeypatch.setattr("fno.pr._quota.record_refusal", _REAL_RECORD_REFUSAL)
    ops = []
    monkeypatch.setattr(
        "fno.pr._quota._gh_budget",
        lambda payload: ops.append(payload.get("op")),
    )
    reason = _rest._rest_reason(
        Result(1, "", _VERBATIM_403), runner=_rate_limit_runner(core_remaining=4980)
    )
    assert reason.rate_limit_class == "secondary"
    assert ops == ["refused"]


def test_a_core_classification_records_nothing(monkeypatch):
    ops = []
    monkeypatch.setattr(
        "fno.pr._quota._gh_budget",
        lambda payload: ops.append(payload.get("op")),
    )
    reason = _rest._rest_reason(
        Result(1, "", "gh: API rate limit exceeded (HTTP 403)"),
        runner=_rate_limit_runner(core_remaining=0),
    )
    assert reason.rate_limit_class == "core"
    assert ops == []


def test_the_local_budget_refusal_line_sends_no_refused_op(monkeypatch):
    """The local `gh budget:` line says `rate limit` on purpose, so the
    classifier reads it as secondary - but it carries neither HTTP 403 nor
    HTTP 429, and the marker gate in record_refusal keys on those, so a
    refusal this fleet manufactured is never recorded as GitHub's."""
    ops = []
    monkeypatch.setattr(
        "fno.pr._quota._gh_budget",
        lambda payload: ops.append(payload.get("op")),
    )
    local = (
        "gh budget: fleet GitHub rate limit held locally (budget: 450/450 points"
        " in 60s | backoff 0s left); this command did not reach GitHub."
    )
    reason = _rest._rest_reason(
        Result(75, "", local), runner=_rate_limit_runner(core_remaining=4980)
    )
    assert reason.rate_limit_class == "secondary"
    assert ops == [], "no refused op may reach the ledger for a local line"


def test_matched_line_is_quoted_not_the_first_line():
    """A multi-line stderr where the classifier matches line 2: the quoted
    evidence is the matched line, not line 1 (the pre-fix code always quoted
    lines[0])."""
    stderr = "gh: warning: something unrelated happened\n" + _VERBATIM_403
    reason = _rest._rest_reason(
        Result(1, "", stderr), runner=_rate_limit_runner(core_remaining=4980)
    )
    assert reason.startswith("gh: API rate limit exceeded")
    assert "something unrelated" not in reason


def test_shim_only_stderr_keeps_the_shims_own_diagnostic_raw():
    """Wrapper noise is excluded from the EVIDENCE only while real gh output
    exists. When the shim's own fatal diagnostic is the whole message, it IS
    the message: quoted verbatim, with no classification (the read died
    before gh ran; binning it as a 404 mislabels a failure gh never
    reported)."""
    reason = _rest._rest_reason(
        Result(1, "", "gh proxy: real gh executable not found")
    )
    assert reason == "gh proxy: real gh executable not found"
    assert not hasattr(reason, "rate_limit_class") or not reason.rate_limit_class


def test_transport_failure_names_its_class_and_disclaims_blockers():
    """x-4eac (the 2026-08-19 EOF incident): a transport death is a fact about
    the READ. The reason must say so before a worker polls harder or edits
    content that was never read."""

    class Res:
        stderr = 'Post "https://api.github.com/graphql": unexpected EOF'
        stdout = ""

    reason = _rest._rest_reason(Res())
    assert "TRANSPORT" in reason
    assert "not a verdict about this PR" in reason
    assert "not content" in reason


def test_auth_failure_names_its_class():
    class Res:
        stderr = "gh: HTTP 401: Bad credentials"
        stdout = ""

    reason = _rest._rest_reason(Res())
    assert "AUTHENTICATION" in reason
    assert "gh auth login" in reason


def test_not_found_names_the_pr_number_as_the_thing_to_check() -> None:
    class Res:
        stderr = "gh: Not Found (https://api.github.com/repos/o/r/pulls/999)"
        stdout = ""

    reason = _rest._rest_reason(Res())
    assert "not found" in reason.lower()
    assert "Check the PR number" in reason


def test_digits_containing_404_are_not_a_not_found() -> None:
    class Res:
        stderr = "gh: run 14045 failed with status 8"
        stdout = ""

    reason = _rest._rest_reason(Res())
    # "1404" as a substring of "14045" must not read as the 404 status;
    # this failure has no class, so it names the raw line and nothing more.
    assert "not found" not in reason.lower()
    assert "Check the PR number" not in reason


def test_bare_404_status_is_a_not_found() -> None:
    class Res:
        stderr = "gh: HTTP 404 (https://api.github.com/repos/o/r/pulls/999)"
        stdout = ""

    reason = _rest._rest_reason(Res())
    assert "not found" in reason.lower()
    assert "Check the PR number" in reason


def test_repo_slug_reason_names_its_failure_class(tmp_path, monkeypatch):
    """A bare None could not tell three different failures apart, and the one
    caller that rendered it said "repo slug unreadable" for all of them.

    That sentence is wrong in subject whenever the SLUG is the readable thing
    and the CWD is what does not exist - the case a caller hits by passing
    `owner/repo` into a parameter that takes a path. Both are bare `str`, so
    nothing static catches it. `isdir` does, before git is spawned (x-51f7).
    """
    monkeypatch.chdir(tmp_path)

    def never_runs(cmd, cwd=None):
        raise AssertionError(f"a non-directory must be refused before any spawn: {cmd}")

    slug, reason = _rest._repo_slug_reason("bllshttng/footnote", never_runs)
    assert slug is None
    assert reason == "no such directory: bllshttng/footnote"

    # A real directory with no origin quotes git's own sentence.
    def no_origin(cmd, cwd=None):
        return Result(1, "", "error: No such remote 'origin'\n")

    slug, reason = _rest._repo_slug_reason(str(tmp_path), no_origin)
    assert slug is None
    assert reason == "error: No such remote 'origin'"

    # A remote that is not GitHub names the url it could not parse.
    def gitlab(cmd, cwd=None):
        return Result(0, "git@gitlab.com:owner/repo.git\n", "")

    slug, reason = _rest._repo_slug_reason(str(tmp_path), gitlab)
    assert slug is None
    assert reason == "origin is not a github remote: git@gitlab.com:owner/repo.git"

    # The success case carries an EMPTY reason, so a caller reads the pair
    # rather than inferring success from a missing sentence.
    def github(cmd, cwd=None):
        return Result(0, _GH_URL, "")

    assert _rest._repo_slug_reason(str(tmp_path), github) == ("owner/repo", "")
    assert _rest._repo_slug(str(tmp_path), github) == "owner/repo"


def test_repo_slug_reason_redacts_credentials_in_the_remote_url():
    """The reason is PUBLISHED: it rides the coverage note into the
    `fno/review-coverage` commit status and into .fno/events.jsonl.

    A CI-shaped clone carries its token in the url's userinfo, and such a
    remote reaches the non-GitHub arm precisely BECAUSE its host is not
    github.com - so the credential is exactly what would get posted.
    """
    def token_remote(cmd, cwd=None):
        return Result(0, "https://x-access-token:ghs_SECRET@ghe.internal/o/r.git\n", "")

    _slug, reason = _rest._repo_slug_reason(".", token_remote)
    assert reason == "origin is not a github remote: https://***@ghe.internal/o/r.git"
    assert "ghs_SECRET" not in reason

    # git's own stderr can quote the url too, so the same redaction guards it.
    def failing_remote(cmd, cwd=None):
        return Result(1, "", "fatal: https://u:p@ghe.internal/o/r.git not found\n")

    _slug, reason = _rest._repo_slug_reason(".", failing_remote)
    assert reason == "fatal: https://***@ghe.internal/o/r.git not found"


def test_slug_or_reason_gives_every_rendering_caller_the_subject(tmp_path, monkeypatch):
    """The refusal the REST readers print keeps its opening and gains its
    subject. Without this the five callers that discard the reason blame the
    remote for a cwd that never existed."""
    monkeypatch.chdir(tmp_path)
    slug, reason = _rest._slug_or_reason("bllshttng/footnote")
    assert slug is None
    assert reason == "could not resolve owner/repo: no such directory: bllshttng/footnote"

    # A caller that already holds a slug short-circuits the git read entirely.
    def never_runs(cmd, cwd=None):
        raise AssertionError(f"a held slug must not spawn git: {cmd}")

    assert _rest._slug_or_reason(None, never_runs, "owner/repo") == ("owner/repo", "")

    # And it reaches the reader an operator actually calls.
    info, reason = _rest.fetch_pr_info_rest("42", cwd="bllshttng/footnote")
    assert info is None
    assert reason == "could not resolve owner/repo: no such directory: bllshttng/footnote"


def test_cwd_refusal_names_which_of_the_three_facts_isdir_hid(tmp_path, monkeypatch):
    """`isdir` is false for a missing path, an existing non-directory, and an
    unreadable one. Asserting "no such directory" for all three is the same
    wrong-subject defect, relocated - the path in the message plainly exists.
    """
    import os

    from fno.pr import _rest as R

    assert R._cwd_refusal(None) == ""
    assert R._cwd_refusal(str(tmp_path)) == ""

    missing = tmp_path / "gone"
    assert R._cwd_refusal(str(missing)) == f"no such directory: {missing}"

    a_file = tmp_path / "origin.txt"
    a_file.write_text("not a directory\n")
    assert R._cwd_refusal(str(a_file)) == f"not a directory: {a_file}"

    def denied(path):
        raise PermissionError(13, "Permission denied")

    monkeypatch.setattr(os, "stat", denied)
    assert R._cwd_refusal(str(tmp_path)) == (
        f"cannot stat: {tmp_path} (Permission denied)"
    )


def test_cwd_refusal_rejects_the_empty_string_that_crashes_a_spawn():
    """subprocess reads "" as a path, not as "the current directory", so it
    raises. The two guards disagreed about it once: one tested `is not None`
    and the other tested truthiness, and "" walked through the gap into every
    probe. One implementation, one answer."""
    assert _rest._cwd_refusal("") == (
        "empty cwd: pass a directory or None, never an empty string"
    )


def test_published_refusal_hides_the_account_name_in_a_local_path(tmp_path, monkeypatch):
    """The refusal rides the coverage note into a GitHub commit status. It
    redacts a token in one clause, so interpolating a home directory raw in
    the next is an oversight, not a policy."""
    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.delenv("USERPROFILE", raising=False)
    reason = _rest._cwd_refusal(str(tmp_path / "code" / "proj"))
    assert reason == "no such directory: ~/code/proj"
    assert str(tmp_path) not in reason




