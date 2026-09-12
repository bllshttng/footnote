"""The Python door of the bot-review producer.

The publish contract itself (gate chain, refusals, the POST, the
reviewDecision readback) lives in the Rust module behind the binary
(``crates/fno-agents/src/publish_review.rs``) and is tested there. These
tests pin the Python side of the seam: the transport round-trip, and the
hidden verb's exit-code mapping.
"""

from __future__ import annotations

import json

from typer.testing import CliRunner

from fno.pr._publish_review import PublishReviewUnavailable, publish_review_call

runner = CliRunner()

ANSWER = {
    "status": "posted",
    "reason": "posted APPROVE as fno-review-bot on #931 (reviewDecision=APPROVED)",
    "event": "APPROVE",
    "review_decision": "APPROVED",
    "stderr": None,
    "receipt": "bot-review: posted APPROVE as fno-review-bot on #931 (reviewDecision=APPROVED)",
    "exit": 0,
}


def test_publish_review_call_round_trips_the_payload(tmp_path, monkeypatch):
    import fno.rust_binary as rb

    seen = {}

    def fake_run(argv, input=None, capture_output=True, text=True, timeout=None):
        seen["argv"] = argv
        seen["input"] = input
        seen["timeout"] = timeout

        class Proc:
            returncode = 0
            stdout = json.dumps(ANSWER)
            stderr = ""

        return Proc()

    monkeypatch.setattr(rb, "find_dev_binary", lambda: tmp_path / "unused")
    # verb_call imports subprocess locally, which resolves to this same module.
    monkeypatch.setattr("subprocess.run", fake_run)
    answer = publish_review_call({"pr_number": 931, "verdict": "pass"})
    assert answer["status"] == "posted"
    assert seen["argv"][-1] == "publish-review"
    assert json.loads(seen["input"])["pr_number"] == 931
    # Real network round trips: the transport bound must exceed the default.
    assert seen["timeout"] == 45


def test_publish_review_call_raises_a_named_refusal_on_failure(tmp_path, monkeypatch):
    import fno.rust_binary as rb

    def fake_run(argv, input=None, capture_output=True, text=True, timeout=None):
        class Proc:
            returncode = 1
            stdout = ""
            stderr = "boom"

        return Proc()

    monkeypatch.setattr(rb, "find_dev_binary", lambda: tmp_path / "unused")
    monkeypatch.setattr("subprocess.run", fake_run)
    try:
        publish_review_call({})
    except PublishReviewUnavailable as exc:
        assert "boom" in str(exc)
    else:
        raise AssertionError("a failing binary must raise the named refusal")


def test_verb_maps_the_answer_exit_code(tmp_path, monkeypatch):
    from fno.pr import cli as pr_cli
    from fno.pr._publish_review import publish_review_call

    monkeypatch.setattr("fno.pr._publish_review.publish_review_call", lambda p: dict(ANSWER))
    result = runner.invoke(pr_cli.pr_app, ["publish-review", "--pr-number", "931"])
    assert result.exit_code == 0, result.output


def test_verb_passes_the_default_verdict_and_dry_run_flag(tmp_path, monkeypatch):
    from fno.pr import cli as pr_cli
    from fno.pr._publish_review import publish_review_call

    seen = {}

    def fake(payload):
        seen.update(payload)
        return {"status": "skipped", "receipt": "bot-review: skipped (x)", "exit": 1}

    monkeypatch.setattr("fno.pr._publish_review.publish_review_call", fake)
    result = runner.invoke(
        pr_cli.pr_app, ["publish-review", "--pr", "5", "--dry-run"]
    )
    assert result.exit_code == 1
    assert seen["pr_number"] == 5
    assert seen["verdict"] == ""
    assert seen["dry_run"] is True
    assert "bot-review: skipped (x)" in result.output


def test_verb_survives_an_unavailable_binary(monkeypatch):
    from fno.pr import cli as pr_cli
    from fno.pr._publish_review import PublishReviewUnavailable

    def fake(payload):
        raise PublishReviewUnavailable("the fno-agents binary was not found")

    monkeypatch.setattr("fno.pr._publish_review.publish_review_call", fake)
    result = runner.invoke(pr_cli.pr_app, ["publish-review", "--pr", "5"])
    assert result.exit_code == 1
    assert "bot-review: failed" in result.output
