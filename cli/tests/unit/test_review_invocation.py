"""Tests for the canonical review invocation telemetry contract."""
from __future__ import annotations

import json
from datetime import datetime, timedelta, timezone

from fno.doctor import _review_invocation_report
from fno.events import validate
from fno.review.invocation import (
    adopt_pending_invocation,
    build_review_invocation_data,
    mint_invocation_id,
    pending_invocation_path,
    write_pending_invocation,
)


def _sent_event() -> dict:
    return {
        "ts": "2026-08-26T00:00:00Z",
        "type": "review_invocation",
        "source": "daemon",
        "data": {
            "invocation_id": "ri-test-1",
            "stage": "sent",
            "verb": "/review",
            "args_raw": "medium --comment",
            "level": "medium",
            "level_source": "explicit",
            "flags": ["--comment"],
            "transport": "mux_pane_send_raw",
            "initiator": "self",
            "target_session_id": "target-test-1",
            "submit_required": True,
            "submit_key": "\\r",
            "submit_confirmed": False,
            "receipt": "text delivered, submission unconfirmed",
            "pr": 42,
            "head_sha": "a" * 40,
            "branch": "feature/review",
        },
    }


def test_review_invocation_event_is_a_valid_canonical_record() -> None:
    event = _sent_event()

    validate(event)

    assert event["type"] == "review_invocation"
    assert event["data"]["receipt"] == "text delivered, submission unconfirmed"


def test_refused_stage_with_reason_is_a_valid_canonical_record() -> None:
    """The empty-diff terminal row validates against the schema.

    emit-attestation.sh journals stage=refused reason=empty_diff when a review
    ran and its measured diff had nothing to read, so the schema must admit
    the stage and the reason field (the emit chokepoint validates every row
    against this schema before writing, so a schema without them would refuse
    the row and re-orphan the attempt it terminates).
    """
    event = _sent_event()
    event["data"]["stage"] = "refused"
    event["data"]["reason"] = "empty_diff"
    event["data"]["reviewed_base_sha"] = "b" * 40
    event["data"]["reviewed_head_sha"] = event["data"]["head_sha"]
    event["data"]["reviewed_file_count"] = 0

    validate(event)


def test_review_invocation_data_omits_unknown_and_keeps_false_and_zero() -> None:
    data = build_review_invocation_data(
        invocation_id="ri-test-2",
        stage="started",
        verb="/review",
        submit_confirmed=False,
        subagent_count=0,
        model_family=None,
    )

    assert data["invocation_id"] == "ri-test-2"
    assert data["submit_confirmed"] is False
    assert data["subagent_count"] == 0
    assert "model_family" not in data


def test_pending_invocation_round_trips_a_positive_join_id(tmp_path) -> None:
    invocation_id = mint_invocation_id()

    assert invocation_id.startswith("ri-")
    assert write_pending_invocation(
        target_session_id="target-test-2",
        invocation_id=invocation_id,
        home=tmp_path,
    ) is True
    assert pending_invocation_path("target-test-2", home=tmp_path).is_file()
    assert adopt_pending_invocation("target-test-2", home=tmp_path) == invocation_id


def test_pending_invocation_write_is_fail_open(tmp_path) -> None:
    blocked_home = tmp_path / "blocked"
    blocked_home.write_text("positive-control", encoding="utf-8")
    invocation_id = mint_invocation_id()

    assert write_pending_invocation(
        target_session_id="target-test-3",
        invocation_id=invocation_id,
        home=blocked_home,
    ) is False
    assert invocation_id.startswith("ri-")


def test_adopt_consumes_so_a_second_send_owns_the_slot(tmp_path) -> None:
    # A stale unconsumed sidecar makes the O_EXCL writer fail forever while
    # senders mint unjoinable ids. Adoption must consume it so the NEXT
    # write succeeds and each review keeps its own id.
    assert write_pending_invocation(
        target_session_id="target-test-4", invocation_id="ri-old", home=tmp_path
    )
    assert (
        write_pending_invocation(
            target_session_id="target-test-4", invocation_id="ri-new", home=tmp_path
        )
        is False
    )
    assert adopt_pending_invocation("target-test-4", home=tmp_path) == "ri-old"
    assert (
        write_pending_invocation(
            target_session_id="target-test-4", invocation_id="ri-new", home=tmp_path
        )
        is True
    )


def test_doctor_survives_a_tz_naive_row_and_names_it_lost(tmp_path) -> None:
    # A naive ts once raised on the cutoff comparison and killed the whole
    # doctor run; it is read as UTC instead.
    path = tmp_path / "events.jsonl"
    path.write_text(
        json.dumps(
            {
                "ts": "2026-08-25T23:44:00",
                "type": "review_invocation",
                "source": "daemon",
                "data": {
                    "invocation_id": "ri-naive",
                    "stage": "sent",
                    "verb": "/review",
                },
            }
        )
        + "\n",
        encoding="utf-8",
    )

    lines = _review_invocation_report(
        path, now=datetime(2026, 8, 26, tzinfo=timezone.utc)
    )

    assert any("ri-naive" in line for line in lines)


def test_doctor_reads_a_missing_journal_as_nothing_lost(tmp_path) -> None:
    lines = _review_invocation_report(
        tmp_path / "absent-events.jsonl", now=datetime(2026, 8, 26, tzinfo=timezone.utc)
    )

    assert any("no event journal" in line for line in lines)


def test_doctor_names_an_old_sent_attempt_with_transport_and_receipt(tmp_path) -> None:
    path = tmp_path / "events.jsonl"
    path.write_text(
        json.dumps(
            {
                "ts": "2026-08-25T23:44:00Z",
                "type": "review_invocation",
                "source": "daemon",
                "data": {
                    "invocation_id": "ri-lost-positive",
                    "stage": "sent",
                    "verb": "/review",
                    "transport": "mux_pane_send_raw",
                    "receipt": "text delivered, submission unconfirmed",
                },
            }
        )
        + "\n",
        encoding="utf-8",
    )

    lines = _review_invocation_report(
        path,
        now=datetime(2026, 8, 26, tzinfo=timezone.utc),
    )

    assert any(
        "ri-lost-positive" in line
        and "mux_pane_send_raw" in line
        and "submission unconfirmed" in line
        for line in lines
    )


def test_doctor_ignores_sent_attempt_older_than_bounded_window(tmp_path) -> None:
    path = tmp_path / "events.jsonl"
    now = datetime(2026, 8, 26, tzinfo=timezone.utc)
    path.write_text(
        json.dumps(
            {
                "ts": (now - timedelta(minutes=31)).isoformat().replace("+00:00", "Z"),
                "type": "review_invocation",
                "source": "daemon",
                "data": {
                    "invocation_id": "ri-too-old",
                    "stage": "sent",
                    "verb": "/review",
                },
            }
        )
        + "\n",
        encoding="utf-8",
    )

    lines = _review_invocation_report(path, now=now)

    assert any("none lost in the last 15m" in line for line in lines)
    assert all("ri-too-old" not in line for line in lines)


def test_doctor_explicitly_reports_no_lost_attempt_after_attestation(tmp_path) -> None:
    path = tmp_path / "events.jsonl"
    rows = [
        {
            "ts": "2026-08-25T00:00:00Z",
            "type": "review_invocation",
            "source": "daemon",
            "data": {"invocation_id": "ri-covered-positive", "stage": "sent"},
        },
        {
            "ts": "2026-08-25T00:01:00Z",
            "type": "review_attestation",
            "source": "hook",
            "data": {"invocation_id": "ri-covered-positive"},
        },
    ]
    path.write_text("".join(json.dumps(row) + "\n" for row in rows), encoding="utf-8")

    lines = _review_invocation_report(
        path,
        now=datetime(2026, 8, 26, tzinfo=timezone.utc),
    )

    assert any("none lost" in line for line in lines)


# --- flag vocabulary: one normalizer serves the router prose and telemetry ---

def test_canonical_flag_accepts_three_spellings() -> None:
    from fno.review.invocation import canonical_flag

    for spelling in ("--comment", "comment", "—comment"):
        assert canonical_flag(spelling) == "--comment", spelling
    for spelling in ("--fix", "fix", "—fix"):
        assert canonical_flag(spelling) == "--fix", spelling


def test_canonical_flag_rejects_every_other_dash() -> None:
    """The em-dash alias is exactly one character. An en dash, a minus sign,
    or a horizontal bar leading the same name is NOT a flag: no legitimate
    target begins with an em dash, but those characters do begin other text,
    and widening the class would capture real targets."""
    from fno.review.invocation import canonical_flag

    for token in ("–comment", "−comment", "―comment", "–fix"):
        assert canonical_flag(token) is None, repr(token)


def test_canonical_flag_rejects_unknown_and_partial_names() -> None:
    from fno.review.invocation import canonical_flag

    assert canonical_flag("commentx") is None
    assert canonical_flag("--verbose") is None
    # one em dash then an UNKNOWN name is prose, not a flag attempt
    assert canonical_flag("—verbose") is None


def test_parse_records_canonical_flags_for_every_spelling() -> None:
    from fno.review.invocation import parse_review_invocation

    bare = parse_review_invocation("/fno:review medium comment fix")
    assert bare is not None
    assert bare["flags"] == ["--comment", "--fix"]
    assert bare["level"] == "medium"

    em = parse_review_invocation("/fno:review medium —comment")
    assert em is not None
    assert em["flags"] == ["--comment"]

    canonical = parse_review_invocation("/fno:review medium --comment")
    assert canonical is not None
    assert canonical["flags"] == ["--comment"]


def test_parse_keeps_unknown_double_hyphen_tokens_verbatim() -> None:
    from fno.review.invocation import parse_review_invocation

    parsed = parse_review_invocation("/fno:review high --verbose")
    assert parsed is not None
    assert parsed["flags"] == ["--verbose"]


def test_parse_emits_a_react_to_em_dash_without_flag_name() -> None:
    """An em dash followed by a non-flag word is left entirely alone: it is
    prose punctuation, and the flags list records nothing."""
    from fno.review.invocation import parse_review_invocation

    parsed = parse_review_invocation("/fno:review high — quick pass")
    assert parsed is not None
    assert parsed["flags"] == []
