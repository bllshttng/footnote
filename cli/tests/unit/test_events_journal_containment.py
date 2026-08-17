"""The checkout event journal is not a test fixture's scratch space.

Measured 2026-08-17: `fno agents needs --json` returned 12 rows, 6 of them test
fixtures wearing production shape (`acme-web -> fno-peer: design Q: which
auth?`). They reached a live operator surface because `append_event` resolves
its default journal from `resolve_repo_root()`, and `fno.hermetic.neutralise`
deliberately leaves `FNO_REPO_ROOT` unset, so the resolver falls through to `git
rev-parse` and finds the developer's real checkout.

The containment is at the writer, never at the fold. A fold that recognises
fixture-shaped rows carries an exception list, and the next fixture that does
not match the list refills the queue in silence.

These tests are the guard the fix exists for. Cleaning today's rows is one-time;
without an assertion the next test touching an escalation path refills the
queue and nobody notices, because a polluted panel looks exactly like a busy
one.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.paths_testing import use_tmpdir


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


@pytest.fixture
def pinned(tmp_path: Path, monkeypatch) -> dict[str, Path]:
    """A fake checkout plus the scratch journal `neutralise` pins beside it.

    The checkout is fake rather than real so the assertion is hermetic and
    still means something in CI, where no polluted 51 MB journal exists to
    detect a write against.
    """
    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.setenv("FNO_INBOX_ROOT", str(tmp_path / "inbox"))
    monkeypatch.setenv("FNO_INBOX_TEST_MODE", "1")  # suppress desktop notify

    checkout = tmp_path / "repo"
    (checkout / ".fno").mkdir(parents=True)
    checkout_journal = checkout / ".fno" / "events.jsonl"
    checkout_journal.write_text("", encoding="utf-8")

    monkeypatch.setenv("FNO_REPO_ROOT", str(checkout))
    # resolve_repo_root freezes the env var at first call (see its docstring),
    # and the autouse conftest clear already ran before this fixture.
    import fno.paths as paths_mod

    paths_mod.resolve_repo_root.cache_clear()

    scratch = tmp_path / "scratch" / "events.jsonl"
    monkeypatch.setenv("FNO_EVENTS_PATH", str(scratch))
    return {"checkout": checkout_journal, "scratch": scratch}


def _rows(journal: Path, event_type: str) -> list[dict]:
    if not journal.exists():
        return []
    out = []
    for line in journal.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        row = json.loads(line)
        if row.get("type") == event_type:
            out.append(row)
    return out


def test_escalation_lands_in_the_scratch_journal_not_the_checkout(pinned, runner) -> None:
    """A `--kind question` send must leave the checkout journal byte-identical
    while the scratch journal gains the row.

    Both halves are load-bearing. The scratch assertion is the positive
    control: without it, a send that never escalated at all (a refactor that
    drops the call, a debounce that swallows it) would satisfy the containment
    half and read as proof of a fix that is not there.
    """
    from fno.mail.cli import mail_app

    before = pinned["checkout"].read_bytes()

    result = runner.invoke(
        mail_app,
        [
            "send",
            "--to-project", "fno-peer",
            "--kind", "question",
            "--from-name", "acme-web",
            "design Q: which auth?",
        ],
    )
    assert result.exit_code == 0, result.output

    escalations = _rows(pinned["scratch"], "mail_escalation")
    assert len(escalations) == 1, f"expected one escalation, got {escalations}"
    assert escalations[0]["data"]["recipient"] == "fno-peer"

    assert pinned["checkout"].read_bytes() == before


def test_ask_writes_under_its_own_root_never_the_shared_pin(pinned, runner) -> None:
    """`fno outstanding ask` is the other writer that reaches an operator
    surface, and it matters more than the mail escalation: `mail_escalation` is
    windowed by the fold's 24h `since` bound, so a fixture row ages out on its
    own, while `operator_question` is exempt and sits there until a human
    clears it by hand.

    It is contained differently. `ask` resolves a root and passes an explicit
    `events_path=`, so it never reaches the unpathed default the pin covers.
    That is the property asserted here, in both directions: the row lands under
    the resolved root, and the shared sandbox journal stays untouched. Drop the
    explicit path and the row moves to the pin, which this test fails on -
    and in production, where no pin is set, that same drop would silently move
    every operator question from the canonical checkout to a worktree.
    """
    from fno.outstanding.cli import outstanding_app

    result = runner.invoke(outstanding_app, ["ask", "which auth?"])
    assert result.exit_code == 0, result.output

    questions = _rows(pinned["checkout"], "operator_question")
    assert len(questions) == 1, f"expected one question, got {questions}"
    assert questions[0]["data"]["question"] == "which auth?"

    assert not pinned["scratch"].exists(), "a rooted writer must not use the pin"


def test_explicit_events_path_outranks_the_pin(pinned, tmp_path) -> None:
    """An explicit `events_path=` still wins, so every test that already passes
    its own tmp journal keeps working unchanged.
    """
    from fno.events import append_event, operator_question

    explicit = tmp_path / "explicit" / "events.jsonl"
    append_event(
        operator_question(question_id="q-1", question="explicit?", session_id=None),
        events_path=explicit,
    )

    assert len(_rows(explicit, "operator_question")) == 1
    assert not pinned["scratch"].exists()
    assert pinned["checkout"].read_bytes() == b""


def test_pinned_journal_parent_is_created_on_demand(pinned) -> None:
    """The pin may name a directory that does not exist yet, matching what the
    repo-relative default has always done for a fresh checkout.
    """
    from fno.events import append_event, operator_question

    assert not pinned["scratch"].parent.exists()
    append_event(operator_question(question_id="q-2", question="fresh?", session_id=None))

    assert len(_rows(pinned["scratch"], "operator_question")) == 1
