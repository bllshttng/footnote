"""The friction lane after the fleet-task port: the contended and
polling_settled verdicts ride ``reconcile_friction``, which is ONE transport
call through ``verb_call("fleet-task", ...)``.

The fold's behavior is characterized in Rust
(``fleet_task_reconcile_parity.rs``); what stays testable here is the lane's
payload: its marker lane, its measured-set empty flag, and the question text
it renders from the verdicts.
"""
from __future__ import annotations

from pathlib import Path

import pytest

_NOW = 1_800_000_000.0


@pytest.fixture(autouse=True)
def isolate_question_index(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(
        "fno.paths.questions_jsonl",
        lambda: tmp_path / "questions.jsonl",
        raising=False,
    )


def _friction_run(root, rows, transcripts, monkeypatch, pr_state_for=None, captured=None):
    """The verb's flow with the fleet-enumeration seams injected and the
    transport stubbed: classification runs for real, the fold's payload is
    captured and a canned outcome comes back."""
    import fno.rust_binary
    from fno.agents import friction_lane as fl
    from fno.agents import watchdog as wd

    def fake_verb_call(verb, payload, *args, **kwargs):
        assert verb == "fleet-task"
        if captured is not None:
            captured.append(payload)
        return {"outcome": "asked", "id": "ft-friction00"}

    import fno.agents.stale_escalate as se

    monkeypatch.setattr(fno.rust_binary, "verb_call", fake_verb_call)

    payload, out_rows = wd.run_sweep(
        now_s=_NOW,
        rows_provider=lambda: (rows, []),
        transcript_fn=lambda sid: transcripts.get(sid),
        claim_fn=lambda node: {},
        graph_fn=lambda: {},
        pr_state_fn=pr_state_for,
    )
    assert not payload.get("refused")
    pairs = [
        (wd.Verdict(**data), row)
        for data, row in zip(payload["verdicts"], out_rows)
        if data["verdict"] in (wd.CONTENDED, wd.POLLING_SETTLED)
    ]
    return fl.reconcile_friction(
        pairs, root=root, session_id="watchdog-test", cwd=root
    )


def _facts(text: str, age_s: float, pr_polls: tuple = ()):
    from fno.agents.watchdog import TailFacts

    return TailFacts(
        [(_NOW - age_s, text)], _NOW - age_s, text, "assistant", text, pr_polls
    )


def _row(sid: str, name: str, cwd: str):
    from fno.agents.watchdog import Row

    return Row(sid, name, "working", None, cwd)


def test_one_question_names_every_friction_row(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    captured: list = []
    wt = str(tmp_path / "w1")
    (tmp_path / "w1").mkdir()
    (tmp_path / "w1" / ".git").write_text("gitdir: /tmp/elsewhere/main\n")
    rows = [
        _row("aaaa1111-0000", "w1", wt),
        _row("bbbb2222-0000", "w2", wt),
        _row("cccc3333-0000", "w3", "/tmp/w3"),
    ]
    transcripts = {
        "aaaa1111-0000": _facts("still on it", 60),
        "bbbb2222-0000": _facts("still on it", 60),
        "cccc3333-0000": _facts(
            "checking", 60,
            (("settled", 42, "MERGED"), ("read", 42, ""), ("read", 42, "")),
        ),
    }
    outcome, qid = _friction_run(
        tmp_path, rows, transcripts, monkeypatch,
        pr_state_for=lambda cwd, n: "MERGED",
        captured=captured,
    )
    assert outcome == "asked"
    assert qid == "ft-friction00"
    [payload] = captured
    assert payload["lane"] == "watchdog-friction"
    assert payload["empty"] is False
    assert "3 contention/polling row(s)" in payload["text"]
    assert "w1" in payload["text"] and "w3" in payload["text"]


def test_emptied_set_sends_the_empty_flag(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    captured: list = []
    wt = str(tmp_path / "w1")
    (tmp_path / "w1").mkdir()
    (tmp_path / "w1" / ".git").write_text("gitdir: /tmp/elsewhere/main\n")
    rows = [_row("aaaa1111-0000", "w1", wt), _row("bbbb2222-0000", "w2", wt)]
    transcripts = {
        "aaaa1111-0000": _facts("still on it", 60),
        "bbbb2222-0000": _facts("still on it", 60),
    }
    _friction_run(tmp_path, rows, transcripts, monkeypatch, captured=captured)
    assert captured, "the first run filed a task"
    # The peer goes quiet-and-finished: one live row in the tree, no friction.
    quiet = dict(transcripts)
    quiet["bbbb2222-0000"] = _facts(
        "<promise>PR is green and reviewed</promise>", 1800
    )
    outcome, _qid = _friction_run(tmp_path, rows, quiet, monkeypatch, captured=captured)
    assert outcome == "asked"
    assert captured[-1]["empty"] is True, captured[-1]
