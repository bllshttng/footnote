"""The ready-for-review draft guard: argv intent, law door, proxy wiring.

Covers `fno/pr/_draft_ready.py` and its delegate() wiring in
`fno/pr/gh_proxy.py`. All decision and config reads are seamed - no test
touches the real decision index or the real settings files. One table test
per surface; each row names its branch.
"""
from __future__ import annotations

import pytest

from fno.pr._draft_ready import (
    DRAFT_DECISION,
    draft_law_authority,
    draft_refusal,
    run_draft_flip,
)

_BRANCH = "feature/x-3159"
_ADMITTING = {
    "open_ready_fn": lambda _cwd: True,
    "authority_fn": lambda _subject: ("none", ""),
    "branch_fn": lambda _cwd: _BRANCH,
}


class TestDraftRefusal:
    """One table per surface: draft_refusal over every argv and authority branch."""

    @pytest.mark.parametrize(
        "argv,seams,expect",
        [
            # create + --draft: refused, subject keyed on the branch, door named.
            (["pr", "create", "--draft"], {}, "pr-draft:" + _BRANCH),
            # --draft=true is intent; --draft=false creates ready and is not.
            (["pr", "create", "--draft=true"], {}, "pr-draft:"),
            (["pr", "create", "--draft=false"], {}, None),
            # ordinary argv pays nothing: no intent, no law read.
            (["pr", "view", "9"], {}, "no-authority-read"),
            (["auth", "status"], {}, "no-authority-read"),
            # an operator ruling admits; an unreadable probe refuses fail-closed.
            (["pr", "create", "--draft"], {"authority_fn": lambda _s: ("single", "")}, None),
            (["pr", "create", "--draft"], {"authority_fn": lambda _s: ("unknown", "probe died")}, "pr-draft:"),
            # the rule off for the repo: the guard stands down.
            (["pr", "create", "--draft"], {"open_ready_fn": lambda _cwd: False}, None),
            # gh DWIM ready with no number: still draft intent, branch subject.
            (["pr", "ready", "--draft"], {}, "pr-draft:" + _BRANCH),
            # a resolvable PR number keys the subject on the repo.
            (["pr", "ready", "7", "--draft"], {}, "pr-draft:owner/repo#7"),
            # an unresolvable branch still names a recordable door.
            (["pr", "create", "--draft"], {"branch_fn": lambda _cwd: None}, "pr-draft:unknown-branch"),
        ],
    )
    def test_refusal_branches(self, monkeypatch, argv, seams, expect):
        merged = dict(_ADMITTING)
        merged.update(seams)
        if expect == "no-authority-read":
            calls = []

            def spy(subject):
                calls.append(subject)
                return "none", ""

            merged["authority_fn"] = spy
            assert draft_refusal(argv, None, **merged) is None
            assert calls == [], "the law read must not run on ordinary argv"
            return
        if argv[:2] == ["pr", "ready"] and "7" in argv:
            monkeypatch.setattr(
                "fno.graph._reconcile.resolve_current_repo_slug",
                lambda _cwd: "owner/repo",
            )
        refusal = draft_refusal(argv, None, **merged)
        if expect is None:
            assert refusal is None
        else:
            assert refusal is not None
            assert expect in refusal
            assert DRAFT_DECISION in refusal
            assert "fno inbox law set" in refusal


class TestDraftLawAuthority:
    """One table: the three-state law read over row shapes and probe health."""

    @pytest.mark.parametrize(
        "list_fn,expect",
        [
            # the affirmative: one operator row carrying the exact decision value.
            (lambda *a, **k: ("", [{"authority_source": "operator", "decision": DRAFT_DECISION}], 0), "single"),
            # a chat_attested row cannot carry a draft exception: clean no.
            (lambda *a, **k: ("", [{"authority_source": "coord", "decision": DRAFT_DECISION}], 0), "none"),
            # row existence carries no polarity: a note at the subject is no.
            (lambda *a, **k: ("", [{"authority_source": "operator", "decision": "a note"}], 0), "none"),
            # conflicting or damaged rows are unknown, never none.
            (lambda *a, **k: ("", [{"authority_source": "operator", "decision": DRAFT_DECISION}] * 2, 0), "unknown"),
            (lambda *a, **k: ("", [], 2), "unknown"),
            # a dead probe is unknown with the fault named.
            (lambda *a, **k: (_ for _ in ()).throw(RuntimeError("index unreadable")), "unknown"),
        ],
    )
    def test_authority_branches(self, list_fn, expect):
        status, probe = draft_law_authority("s", list_fn=list_fn)
        assert status == expect
        if expect == "unknown" and "RuntimeError" in str(probe):
            assert "index unreadable" in probe


class TestConfigBlock:
    def test_open_ready_default_and_quoted_coercion(self):
        from fno.config import PrBlock

        assert PrBlock().open_ready is True
        assert PrBlock(open_ready="false").open_ready is False
        assert PrBlock(open_ready="true").open_ready is True
        assert PrBlock(open_ready=False).open_ready is False


class TestProxyWiring:
    @pytest.mark.parametrize(
        "argv,stub_refusal,expect_exit,expect_exec",
        [
            # draft intent with a refusal: exit 2, the real gh never execs.
            (["pr", "create", "--draft"], "refused: open ready", 2, False),
            # non-draft argv passes the guard and reaches the execve sentinel.
            (["auth", "status"], None, None, True),
        ],
    )
    def test_delegate_guard_branches(self, monkeypatch, capsys, argv, stub_refusal, expect_exit, expect_exec):
        import fno.pr._draft_ready as dr
        import fno.pr.gh_proxy as gh_proxy

        monkeypatch.setattr("fno.pr.gh_proxy._quota.admit", lambda args: None)
        monkeypatch.setattr(
            "fno.pr.gh_proxy._quota.delegate_environment", lambda: {"PATH": "/real/bin"}
        )
        exec_calls = []
        monkeypatch.setattr(gh_proxy.os, "execve", lambda *a: exec_calls.append(a))
        if stub_refusal is not None:
            monkeypatch.setattr(dr, "draft_refusal", lambda args, cwd: stub_refusal)
        else:
            monkeypatch.setattr(
                dr, "draft_refusal", lambda args, cwd: None if argv[:2] != ["pr", "create"] else None
            )
        if expect_exit is not None:
            with pytest.raises(SystemExit) as exc:
                gh_proxy.delegate("/real/gh", argv)
            assert exc.value.code == expect_exit
            assert stub_refusal in capsys.readouterr().err
            assert exec_calls == [], "the real gh must never exec under a draft refusal"
        else:
            def execve_sentinel(path, argv2, env):
                raise RuntimeError("exec sentinel")

            monkeypatch.setattr(gh_proxy.os, "execve", execve_sentinel)
            with pytest.raises(RuntimeError, match="exec sentinel"):
                gh_proxy.delegate("/real/gh", argv)
            assert expect_exec


class TestRunDraftFlip:
    """One table: the sweep flip over its outcome branches."""

    @staticmethod
    def _cand(pr_number=7, slug="owner/repo", node_id="x-abc12345", repo_dir=None):
        from fno.pr_watch._discover import PrCandidate

        return PrCandidate(
            node_id=node_id,
            pr_number=pr_number,
            pr_url=f"https://github.com/{slug}/pull/{pr_number}",
            repo_dir=repo_dir,
            repo_slug=slug,
        )

    @staticmethod
    def _obs(is_draft=True):
        from fno.pr_watch._discover import PrObservation

        return PrObservation(
            pr_number=7, state="OPEN", latest_review_ts=None, opened_at=None, is_draft=is_draft
        )

    @pytest.mark.parametrize(
        "ruling,open_ready,run,expect_receipt,expect_event",
        [
            # flipped: gh pr ready pinned to the candidate's repo, event journaled.
            ("none", True, "ok", "flipped", ("pr_watch_draft_flip", "flipped")),
            # an operator ruling spares the draft with no call and no event.
            ("single", True, "never", "spared", None),
            # gh refusing is an error event and a receipt, never a raise.
            ("none", True, "refused", "flip refused", ("pr_watch_draft_flip", "error")),
            # open_ready=false leaves the draft alone without running gh.
            ("none", False, "never", "open_ready=false", None),
        ],
    )
    def test_flip_branches(self, ruling, open_ready, run, expect_receipt, expect_event):
        events = []
        calls = []

        def runner(cmd, **kwargs):
            calls.append(list(cmd))
            if run == "refused":
                return type("R", (), {"returncode": 1, "stdout": "", "stderr": "not a draft"})()
            return type("R", (), {"returncode": 0, "stdout": "", "stderr": ""})()

        receipt = run_draft_flip(
            self._cand(),
            self._obs(),
            emit=lambda t, d: events.append((t, d)),
            runner=runner,
            ruling_fn=lambda _s: (ruling, ""),
            open_ready_fn=lambda _cwd: open_ready,
        )
        assert expect_receipt in receipt
        if expect_event is None:
            assert events == []
        else:
            got = [(t, {k: v for k, v in d.items() if k != "error"}) for t, d in events]
            assert got == [
                (
                    expect_event[0],
                    {"pr": 7, "repo": "owner/repo", "node": "x-abc12345", "outcome": expect_event[1]},
                )
            ]
        if run == "never":
            assert calls == []
        else:
            assert calls == [["gh", "pr", "ready", "7", "--repo", "owner/repo"]]
