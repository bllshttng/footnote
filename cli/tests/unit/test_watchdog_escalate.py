"""The watchdog's unfinished-work findings become one fleet task.

After the fleet-task port, the fold behind ``escalate_unfinished``
lives in ``crates/fno-agents/src/fleet_task.rs`` and the channel is ONE
transport call. The fold's behavior is characterized in Rust
(``fleet_task_reconcile_parity.rs``); what stays testable here is the text
the lane renders and the payload it sends.
"""
from __future__ import annotations

import importlib
from pathlib import Path

import pytest


@pytest.fixture(autouse=True)
def isolate_question_index(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(
        "fno.paths.questions_jsonl",
        lambda: tmp_path / "questions.jsonl",
        raising=False,
    )


def _subject():
    try:
        return importlib.import_module("fno.agents.stale_escalate")
    except ModuleNotFoundError:
        pytest.fail("fno.agents.stale_escalate is missing")


def _finding(node: str = "x-7d02", *, basis: str = "in_progress, claim free, idle 116h"):
    from fno.agents import unfinished_work as uw

    return uw.Finding(
        kind=uw.KIND_STARTED,
        subject=node,
        basis=basis,
        clear_command=f"/fno:target {node}",
        node_id=node,
        age_s=116 * 3600,
    )


def _run(root: Path, findings, monkeypatch=None, captured=None):
    if monkeypatch is not None:
        import fno.rust_binary

        def fake_verb_call(verb, payload, *args, **kwargs):
            # The stub folds like the Rust door, deriving its state from the
            # shared capture list so it survives across _run calls: the same
            # (lane, key) twice reads duplicate, an empty set reads none.
            assert verb == "fleet-task", verb
            if captured is not None:
                captured.append(payload)
            earlier = captured[:-1] if captured else []
            if payload["empty"]:
                any_open = any(not p["empty"] for p in earlier)
                outcome, id_out = ("closed", "") if any_open else ("none", "")
            else:
                prior = [
                    p
                    for p in earlier
                    if not p["empty"]
                    and (p["lane"], p["key"]) == (payload["lane"], payload["key"])
                ]
                outcome, id_out = ("duplicate", "ft-watchd00") if prior else ("asked", "ft-watchd00")
            return {"outcome": outcome, "id": id_out}

        monkeypatch.setattr(fno.rust_binary, "verb_call", fake_verb_call)
    return _subject().escalate_unfinished(
        findings,
        root=root,
        session_id="watchdog-test",
        cwd=root,
    )


def test_same_finding_set_keys_on_identity(tmp_path: Path, monkeypatch) -> None:
    captured: list = []
    findings = [_finding("x-a"), _finding("x-b")]

    first_outcome, first_id = _run(tmp_path, findings, monkeypatch, captured)
    second_outcome, second_id = _run(tmp_path, list(reversed(findings)), monkeypatch, captured)

    assert first_outcome == "recorded"
    assert second_outcome == "duplicate"
    assert second_id == first_id
    subject = _subject()
    key = subject.dedupe_key([f"{f.kind}:{f.subject}" for f in findings])
    assert captured[0]["key"] == key
    assert captured[0]["lane"] == subject.MARKER


def test_question_names_the_clearing_verbs_not_session_rows() -> None:
    subject = _subject()
    findings = [_finding("x-7d02"), _finding("x-3b05")]
    key = subject.dedupe_key(
        [f"{f.kind}:{f.subject}" for f in findings]
    )
    text = subject.question_text(findings, key)

    assert f"[{subject.MARKER}:{key}]" in text
    assert "unfinished-work finding(s)" in text
    assert "/fno:target x-7d02" in text
    assert "/fno:target x-3b05" in text
    # The retired session-bookkeeping phrasing must not survive the rewrite.
    assert "stale row" not in text
    assert "--only stale" not in text
    assert "reap it or resume it" not in text


def test_identity_change_reasks() -> None:
    subject = _subject()
    first = [_finding("x-a")]
    second = [_finding("x-b")]

    key_one = subject.dedupe_key([f"{f.kind}:{f.subject}" for f in first])
    key_two = subject.dedupe_key([f"{f.kind}:{f.subject}" for f in second])
    assert key_one != key_two


def test_large_finding_set_keeps_marker_count_and_cap(tmp_path: Path, monkeypatch) -> None:
    captured: list = []
    findings = [_finding(f"x-{i:04d}") for i in range(150)]

    first_outcome, first_id = _run(tmp_path, findings, monkeypatch, captured)
    second_outcome, _second_id = _run(tmp_path, list(reversed(findings)), monkeypatch, captured)

    assert first_outcome == "recorded"
    assert second_outcome == "duplicate"
    assert first_id
    assert "150 unfinished-work finding(s)" in captured[0]["text"]
    assert captured[0]["text"].startswith(f"[{_subject().MARKER}:")


def test_empty_finding_set_is_a_named_noop(tmp_path: Path, monkeypatch) -> None:
    captured: list = []
    assert _run(tmp_path, [], monkeypatch, captured) == ("none", "")
    assert captured[0]["empty"] is True
