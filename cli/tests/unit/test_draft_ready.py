"""The ready-for-review draft guard: argv intent, law door, proxy wiring.

Covers `fno/pr/_draft_ready.py` and its delegate() wiring in
`fno/pr/gh_proxy.py`. All decision and config reads are seamed - no test
touches the real decision index or the real settings files.
"""
from __future__ import annotations

import pytest

from fno.pr._draft_ready import (
    DRAFT_DECISION,
    draft_argv_intent,
    draft_law_authority,
    draft_refusal,
    draft_subject_for_pr,
    run_draft_flip,
)


def _admitting(**overrides):
    """Seams that admit: open_ready on, authority none, branch fixed."""
    seams = {
        "open_ready_fn": lambda _cwd: True,
        "authority_fn": lambda _subject: ("none", ""),
        "branch_fn": lambda _cwd: "feature/x-3159",
    }
    seams.update(overrides)
    return seams


class TestDraftArgvIntent:
    def test_create_with_draft_flag_is_create_intent(self):
        assert draft_argv_intent(["pr", "create", "--title", "t", "--draft"]) == (
            True,
            "create",
            None,
        )

    def test_create_with_draft_true_is_intent_and_false_is_not(self):
        assert draft_argv_intent(["pr", "create", "--draft=true"])[0] is True
        assert draft_argv_intent(["pr", "create", "--draft=false"])[0] is False

    def test_ready_with_number_is_ready_intent(self):
        assert draft_argv_intent(["pr", "ready", "7", "--draft"]) == (True, "ready", 7)

    def test_numberless_ready_dwim_is_still_draft_intent(self):
        """`gh pr ready --draft` DWIMs against the current branch's PR; the
        subject falls back to the branch so the refusal still names a door."""
        assert draft_argv_intent(["pr", "ready", "--draft"]) == (True, "ready", None)
        refusal = draft_refusal(
            ["pr", "ready", "--draft"],
            None,
            **_admitting(),
        )
        assert refusal is not None
        assert "pr-draft:feature/x-3159" in refusal

    def test_non_draft_and_non_pr_commands_are_not_intent(self):
        assert draft_argv_intent(["pr", "create", "--title", "t"])[0] is False
        assert draft_argv_intent(["pr", "view", "9", "--draft"])[0] is False
        assert draft_argv_intent(["auth", "status"])[0] is False
        assert draft_argv_intent([])[0] is False


class TestDraftRefusal:
    def test_create_draft_refused_by_default_and_names_the_door(self):
        refusal = draft_refusal(["pr", "create", "--draft"], None, **_admitting())
        assert refusal is not None
        assert "pr-draft:feature/x-3159" in refusal
        assert DRAFT_DECISION in refusal
        assert "fno inbox law set" in refusal

    def test_operator_ruling_admits_the_draft(self):
        refusal = draft_refusal(
            ["pr", "create", "--draft"],
            None,
            **_admitting(authority_fn=lambda _s: ("single", "")),
        )
        assert refusal is None

    def test_unknown_authority_refuses_fail_closed(self):
        refusal = draft_refusal(
            ["pr", "create", "--draft"],
            None,
            **_admitting(authority_fn=lambda _s: ("unknown", "probe died")),
        )
        assert refusal is not None

    def test_ready_draft_keys_the_subject_on_the_pr(self, monkeypatch):
        monkeypatch.setattr(
            "fno.graph._reconcile.resolve_current_repo_slug", lambda _cwd: "owner/repo"
        )
        seen = {}

        def authority(subject):
            seen["subject"] = subject
            return "none", ""

        refusal = draft_refusal(
            ["pr", "ready", "7", "--draft"], None, authority_fn=authority
        )
        assert refusal is not None
        assert seen["subject"] == "pr-draft:owner/repo#7"

    def test_open_ready_false_disables_the_guard(self):
        refusal = draft_refusal(
            ["pr", "create", "--draft"],
            None,
            **_admitting(open_ready_fn=lambda _cwd: False),
        )
        assert refusal is None

    def test_no_draft_intent_costs_nothing(self):
        calls = []

        def spy_authority(subject):
            calls.append(subject)
            return "none", ""

        assert draft_refusal(["pr", "view", "9"], None, authority_fn=spy_authority) is None
        assert calls == [], "the law read must not run on ordinary argv"

    def test_branch_falls_back_to_unknown_branch_marker(self):
        refusal = draft_refusal(
            ["pr", "create", "--draft"],
            None,
            **_admitting(branch_fn=lambda _cwd: None),
        )
        assert "pr-draft:unknown-branch" in refusal


class TestDraftLawAuthority:
    def test_single_operator_row_with_the_affirmative_value(self):
        rows = [
            {"authority_source": "operator", "decision": DRAFT_DECISION},
        ]
        status, probe = draft_law_authority("pr-draft:o/r#7", list_fn=lambda *a, **k: ("", rows, 0))
        assert (status, probe) == ("single", "")

    def test_chat_attested_row_is_a_clean_no(self):
        rows = [{"authority_source": "coord", "decision": DRAFT_DECISION}]
        status, _ = draft_law_authority("s", list_fn=lambda *a, **k: ("", rows, 0))
        assert status == "none"

    def test_row_without_polarity_is_none(self):
        rows = [{"authority_source": "operator", "decision": "a note, not a grant"}]
        status, _ = draft_law_authority("s", list_fn=lambda *a, **k: ("", rows, 0))
        assert status == "none"

    def test_conflict_and_damage_and_dead_probe_are_unknown(self):
        two = [{"authority_source": "operator", "decision": DRAFT_DECISION}] * 2
        assert draft_law_authority("s", list_fn=lambda *a, **k: ("", two, 0))[0] == "unknown"
        assert draft_law_authority("s", list_fn=lambda *a, **k: ("", [], 2))[0] == "unknown"

        def dead(*_a, **_k):
            raise RuntimeError("index unreadable")

        status, probe = draft_law_authority("s", list_fn=dead)
        assert status == "unknown"
        assert "RuntimeError" in probe


class TestConfigBlock:
    def test_default_is_on_and_quoted_values_coerce(self):
        from fno.config import PrBlock

        assert PrBlock().open_ready is True
        assert PrBlock(open_ready="false").open_ready is False
        assert PrBlock(open_ready="true").open_ready is True
        assert PrBlock(open_ready=False).open_ready is False


class TestProxyWiring:
    def test_delegate_refuses_draft_intent_before_exec(self, monkeypatch, capsys):
        import fno.pr._draft_ready as dr
        import fno.pr.gh_proxy as gh_proxy

        monkeypatch.setattr(
            dr, "draft_refusal", lambda args, cwd: "refused: open ready"
        )
        exec_calls = []
        monkeypatch.setattr(gh_proxy.os, "execve", lambda *a: exec_calls.append(a))
        monkeypatch.setattr(
            "fno.pr.gh_proxy._quota.delegate_environment", lambda: {"PATH": "/real/bin"}
        )
        with pytest.raises(SystemExit) as exc:
            gh_proxy.delegate("/real/gh", ["pr", "create", "--draft"])
        assert exc.value.code == 2
        assert "refused: open ready" in capsys.readouterr().err
        assert exec_calls == [], "the real gh must never exec under a draft refusal"

    def test_delegate_admits_non_draft_argv_through_the_guard(self, monkeypatch):
        import fno.pr.gh_proxy as gh_proxy

        monkeypatch.setattr("fno.pr.gh_proxy._quota.admit", lambda args: None)
        monkeypatch.setattr(
            "fno.pr.gh_proxy._quota.delegate_environment", lambda: {"PATH": "/real/bin"}
        )

        def execve(path, argv, env):
            raise RuntimeError("exec sentinel")

        monkeypatch.setattr(gh_proxy.os, "execve", execve)
        with pytest.raises(RuntimeError, match="exec sentinel"):
            gh_proxy.delegate("/real/gh", ["auth", "status"])


class TestRunDraftFlip:
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

    def test_flip_runs_gh_pr_ready_and_journals(self):
        events = []
        calls = []

        def runner(cmd, **kwargs):
            calls.append(list(cmd))

            class R:
                returncode = 0
                stdout = ""
                stderr = ""

            return R()

        receipt = run_draft_flip(
            self._cand(),
            self._obs(),
            emit=lambda t, d: events.append((t, d)),
            runner=runner,
            ruling_fn=lambda _s: ("none", ""),
        )
        assert receipt == "flipped"
        assert calls == [["gh", "pr", "ready", "7", "--repo", "owner/repo"]]
        assert events == [
            (
                "pr_watch_draft_flip",
                {"pr": 7, "repo": "owner/repo", "node": "x-abc12345", "outcome": "flipped"},
            )
        ]

    def test_operator_ruling_spares_the_draft_without_a_call(self):
        events = []
        calls = []
        receipt = run_draft_flip(
            self._cand(),
            self._obs(),
            emit=lambda t, d: events.append((t, d)),
            runner=lambda cmd, **k: calls.append(cmd),
            ruling_fn=lambda _s: ("single", ""),
        )
        assert "spared" in receipt
        assert calls == []
        assert events == []

    def test_gh_refusal_is_an_error_event_never_a_raise(self):
        events = []

        def runner(cmd, **kwargs):
            class R:
                returncode = 1
                stdout = ""
                stderr = "not a draft"

            return R()

        receipt = run_draft_flip(
            self._cand(),
            self._obs(),
            emit=lambda t, d: events.append((t, d)),
            runner=runner,
            ruling_fn=lambda _s: ("none", ""),
        )
        assert receipt.startswith("flip refused")
        assert events[0][1]["outcome"] == "error"

    def test_open_ready_false_leaves_the_draft_alone(self):
        events = []
        receipt = run_draft_flip(
            self._cand(),
            self._obs(),
            emit=lambda t, d: events.append((t, d)),
            runner=lambda cmd, **k: (_ for _ in ()).throw(AssertionError("must not run gh")),
            ruling_fn=lambda _s: ("none", ""),
            open_ready_fn=lambda _cwd: False,
        )
        assert "open_ready=false" in receipt
        assert events == []

    def test_subject_shape_matches_the_guard(self):
        assert draft_subject_for_pr("owner/repo", 7) == "pr-draft:owner/repo#7"
